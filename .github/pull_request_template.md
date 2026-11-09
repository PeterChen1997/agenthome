## What and why / 改了什么，为什么

<!-- Link related issues: Closes #123 / 关联 issue -->

## How it was verified / 如何验证

- [ ] `cargo fmt --all --check`
- [ ] `cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `cargo test --workspace`
- [ ] UI change: launched via `script/bundle-dev.sh` + `open`, screenshots attached below / UI 改动：已用 `bundle-dev.sh` + `open` 启动并附截图

<!-- Screenshots (redact real session titles / project names) / 截图（遮盖真实会话标题与项目名） -->

## Checklist / 自查

- [ ] Fixtures and screenshots contain no real session content, home paths, or secrets / fixture 与截图不含真实会话、家目录路径或密钥
- [ ] AgentHome still never writes to agent directories or reads credentials / 仍然不写 agent 目录、不读凭据
- [ ] If `SCHEMA_VERSION` changed, it is called out above / 如改了 `SCHEMA_VERSION`，已在上方说明
