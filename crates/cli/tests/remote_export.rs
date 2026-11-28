use std::process::Command;

use agenthome_core::device::DeviceIdentity;
use agenthome_core::remote_ssh::{ExportPage, request_command};
use agenthome_core::usage_store::UsageStore;
use rusqlite::Connection;

#[test]
fn ssh_export_reads_while_collector_holds_the_wal_write_lock() {
    let data = tempfile::tempdir().unwrap();
    let identity =
        DeviceIdentity::load_or_create(&data.path().join("device.json"), "runner").unwrap();
    let db = data.path().join("usage/usage.db");
    let store = UsageStore::open(&db, &identity).unwrap();
    let writer = Connection::open(&db).unwrap();
    writer.execute_batch("BEGIN IMMEDIATE").unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_agenthome"))
        .args(["ssh-export", "--data-dir"])
        .arg(data.path())
        .env("SSH_ORIGINAL_COMMAND", request_command(0))
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "ssh-export exited {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let page = ExportPage::decode(&output.stdout).unwrap();
    assert_eq!(page.store_id, store.store_id());
    assert_eq!(page.page.machine_id, identity.machine_id());
    assert_eq!((page.after, page.next, page.more), (0, 0, false));
}
