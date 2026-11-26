#!/usr/bin/env python3
"""Generate AgentHome's USD/1M token price table from two local snapshots."""

import argparse
from datetime import date
from decimal import Decimal
import hashlib
import json
from pathlib import Path
import re


SCRIPT_DIR = Path(__file__).resolve().parent
LITELLM_FIELDS = {
    "input": "input_cost_per_token",
    "output": "output_cost_per_token",
    "cache_read": "cache_read_input_token_cost",
    "cache_write_5m": "cache_creation_input_token_cost",
    "cache_write_1h": "cache_creation_input_token_cost_above_1hr",
}
CONTEXT_FIELD = re.compile(r"input_cost_per_token(_above_(\d+)k_tokens)")
ZERO = Decimal(0)


def snapshot(path):
    data = path.read_bytes()
    return json.loads(data, parse_float=Decimal), hashlib.sha256(data).hexdigest()


def rates(data, base=None):
    """Fill missing cache fields without treating an explicit zero as missing."""
    base = base or {}
    return {
        "input": data["input"],
        "output": data["output"],
        "cache_read": data.get("cache_read", base.get("cache_read", data["input"])),
        "cache_write_5m": data.get("cache_write_5m", base.get("cache_write_5m", ZERO)),
        "cache_write_1h": data.get("cache_write_1h", base.get("cache_write_1h", ZERO)),
    }


def has_token_rate(row):
    return all(
        type(row.get(key)) in (int, Decimal) and row[key] >= 0
        for key in ("input", "output")
    )


def litellm_rate(entry):
    if entry.get("mode") not in ("chat", "responses"):
        return None

    def token_rates(suffix):
        result = {
            key: entry[field + suffix] * 1_000_000
            for key, field in LITELLM_FIELDS.items()
            if entry.get(field + suffix) is not None
        }
        # Some providers use a second spelling for the same cache hit rate.
        cache_hit = entry.get("input_cost_per_token_cache_hit" + suffix)
        if cache_hit is not None:
            result.setdefault("cache_read", cache_hit * 1_000_000)
        return result

    base = token_rates("")
    if not has_token_rate(base) or base["input"] == base["output"] == 0:
        return None
    price = rates(base)
    tiers = [match.groups() for field in entry if (match := CONTEXT_FIELD.fullmatch(field))]
    if len(tiers) > 1:
        raise ValueError("multiple LiteLLM context tiers")
    if tiers:
        suffix, threshold = tiers[0]
        price["long_context"] = {
            "above_tokens": int(threshold) * 1000,
            **rates(token_rates(suffix), price),
        }
    return price


def models_dev_rate(entry):
    cost = entry.get("cost", {})
    if not has_token_rate(cost) or cost["input"] == cost["output"] == 0:
        return None

    def token_rates(row):
        return {**row, "cache_write_5m": row["cache_write"]} if "cache_write" in row else row

    price = rates(token_rates(cost))
    context = {
        row["tier"]["size"]: row for row in cost.get("tiers", [])
        if row["tier"]["type"] == "context"
    }
    if "context_over_200k" in cost:
        context.setdefault(200_000, cost["context_over_200k"])
    if len(context) > 1:
        raise ValueError("multiple models.dev context tiers")
    if context:
        threshold, row = next(iter(context.items()))
        fields = {**price, **token_rates(row)}
        price["long_context"] = {"above_tokens": threshold, **rates(fields)}
    return price


def select_price(name, sources, litellm, models_dev):
    provider = sources.get("models_dev")
    if name.endswith("-free"):
        cost = models_dev.get(provider, {}).get("models", {}).get(name, {}).get("cost", {})
        source = "free tier ($0)"
        if cost.get("input") == cost.get("output") == 0:
            source = f"models.dev/{provider}/{name}; {source}"
        return rates({"input": ZERO, "output": ZERO}), source

    key = sources.get("litellm", name)
    if price := litellm_rate(litellm.get(key, {})):
        return price, f"LiteLLM/{key}"
    if "litellm" not in sources:
        matches = [
            (key, price)
            for key, entry in sorted(litellm.items())
            if key.rsplit("/", 1)[-1].lower() == name and (price := litellm_rate(entry))
        ]
        if matches and all(price == matches[0][1] for _, price in matches):
            key, price = matches[0]
            return price, f"LiteLLM/{key}"

    entries = [provider] if provider else sorted(models_dev)
    matches = [
        (key, price)
        for key in entries
        if (entry := models_dev.get(key, {}).get("models", {}).get(name))
        and (price := models_dev_rate(entry))
    ]
    if matches and all(price == matches[0][1] for _, price in matches):
        key, price = matches[0]
        return price, f"models.dev/{key}/{name}"
    raise ValueError("no unambiguous token price; select a source entry or leave unpriced")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("litellm", type=Path)
    parser.add_argument("models_dev", type=Path)
    parser.add_argument("--date", type=date.fromisoformat, required=True)
    parser.add_argument("--base", type=Path, help="preserve prices from a previous AgentHome table")
    parser.add_argument("--inventory", type=Path, default=SCRIPT_DIR / "pricing-models.json")
    parser.add_argument(
        "--output", type=Path, default=SCRIPT_DIR.parent / "crates/core/src/pricing.json",
    )
    args = parser.parse_args()
    litellm, litellm_hash = snapshot(args.litellm)
    models_dev, models_dev_hash = snapshot(args.models_dev)
    inventory = json.loads(args.inventory.read_text(encoding="utf-8"))

    base, base_hash = snapshot(args.base) if args.base else ({"models": {}}, None)
    models = base["models"]
    patches = [f"preserved earlier prices (AgentHome table SHA-256 {base_hash})"] if args.base else []
    for name in sorted(inventory.keys() - models.keys()):
        try:
            price, source = select_price(name, inventory[name], litellm, models_dev)
        except (KeyError, ValueError) as error:
            parser.error(f"{name}: {error}")
        models[name] = price
        if source != f"LiteLLM/{name}":
            patches.append(f"{name} = {source}")
    table = {
        "_source": (
            "LiteLLM model_prices_and_context_window.json "
            f"(raw.githubusercontent.com/BerriAI/litellm/main; SHA-256 {litellm_hash}); "
            f"models.dev/api.json (SHA-256 {models_dev_hash})"
        ),
        "_fetched": args.date.isoformat(),
        "_patched": "; ".join(patches),
        "_unit": "USD per 1M tokens; cache_write_1h=0 means the provider has no 1h tier (OpenAI); cache_read falls back to input when no cached price is available",
        "_long_context": "long_context: when one request's prompt (input + cache_read + cache_write) exceeds above_tokens, every component of that request uses these rates (standard tier only)",
        "models": dict(sorted(models.items())),
    }
    result = json.dumps(table, indent=1, default=float, allow_nan=False)
    args.output.write_text(result + "\n", encoding="utf-8")


if __name__ == "__main__":
    main()
