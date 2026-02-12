use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tracing::debug;
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("VM not found: {0}")]
    VmNotFound(Uuid),
    #[error("VM not found by name: {0}")]
    VmNotFoundByName(String),
    #[error("VM already exists: {0}")]
    VmAlreadyExists(String),
    #[error("port already forwarded: {0}")]
    PortAlreadyForwarded(u16),
    #[error("lock poisoned")]
    LockPoisoned,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("corrupt data in column {column}: {message}")]
    CorruptData {
        column: &'static str,
        message: String,
    },
}

/// Persistent VM record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VmRecord {
    pub id: Uuid,
    pub name: String,
    pub state: String,
    pub pid: Option<u32>,
    pub vcpu_count: u32,
    pub mem_size_mib: u32,
    pub vsock_cid: u32,
    pub tap_device: Option<String>,
    pub host_ip: Option<String>,
    pub guest_ip: Option<String>,
    pub kernel_path: String,
    pub rootfs_path: String,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Persistent port forward record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortForwardRecord {
    pub id: i64,
    pub vm_id: Uuid,
    pub host_port: u16,
    pub guest_port: u16,
    pub protocol: String,
    pub created_at: DateTime<Utc>,
}

/// SQLite-backed state store. Thread-safe via internal Mutex.
pub struct StateStore {
    conn: Mutex<Connection>,
}

impl StateStore {
    /// Open or create the state database.
    pub fn open(path: &Path) -> Result<Self, StateError> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    /// Open an in-memory database (for testing).
    pub fn open_memory() -> Result<Self, StateError> {
        let conn = Connection::open_in_memory()?;
        let store = Self {
            conn: Mutex::new(conn),
        };
        store.migrate()?;
        Ok(store)
    }

    fn lock(&self) -> Result<std::sync::MutexGuard<'_, Connection>, StateError> {
        self.conn.lock().map_err(|_| StateError::LockPoisoned)
    }

    fn migrate(&self) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS vms (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL UNIQUE,
                state TEXT NOT NULL DEFAULT 'creating',
                pid INTEGER,
                vcpu_count INTEGER NOT NULL,
                mem_size_mib INTEGER NOT NULL,
                vsock_cid INTEGER NOT NULL,
                tap_device TEXT,
                host_ip TEXT,
                guest_ip TEXT,
                kernel_path TEXT NOT NULL,
                rootfs_path TEXT NOT NULL,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS cid_allocator (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                next_cid INTEGER NOT NULL DEFAULT 3
            );

            INSERT OR IGNORE INTO cid_allocator (id, next_cid) VALUES (1, 3);

            CREATE TABLE IF NOT EXISTS freed_cids (
                cid INTEGER PRIMARY KEY
            );

            CREATE TABLE IF NOT EXISTS port_forwards (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                vm_id TEXT NOT NULL REFERENCES vms(id) ON DELETE CASCADE,
                host_port INTEGER NOT NULL UNIQUE,
                guest_port INTEGER NOT NULL,
                protocol TEXT NOT NULL DEFAULT 'tcp',
                created_at TEXT NOT NULL
            );",
        )?;
        Ok(())
    }

    /// Insert a new VM record.
    ///
    /// Returns `StateError::VmAlreadyExists` if a VM with the same name exists.
    pub fn insert_vm(&self, record: &VmRecord) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO vms (id, name, state, pid, vcpu_count, mem_size_mib, vsock_cid,
                              tap_device, host_ip, guest_ip, kernel_path, rootfs_path,
                              created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                record.id.to_string(),
                record.name,
                record.state,
                record.pid,
                record.vcpu_count,
                record.mem_size_mib,
                record.vsock_cid,
                record.tap_device,
                record.host_ip,
                record.guest_ip,
                record.kernel_path,
                record.rootfs_path,
                record.created_at.to_rfc3339(),
                record.updated_at.to_rfc3339(),
            ],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                StateError::VmAlreadyExists(record.name.clone())
            }
            _ => StateError::Database(e),
        })?;
        Ok(())
    }

    /// Get a VM by its ID.
    pub fn get_vm(&self, id: Uuid) -> Result<VmRecord, StateError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, state, pid, vcpu_count, mem_size_mib, vsock_cid,
                    tap_device, host_ip, guest_ip, kernel_path, rootfs_path,
                    created_at, updated_at
             FROM vms WHERE id = ?1",
            params![id.to_string()],
            row_to_vm_record,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StateError::VmNotFound(id),
            other => StateError::Database(other),
        })
    }

    /// Get a VM by name.
    pub fn get_vm_by_name(&self, name: &str) -> Result<VmRecord, StateError> {
        let conn = self.lock()?;
        conn.query_row(
            "SELECT id, name, state, pid, vcpu_count, mem_size_mib, vsock_cid,
                    tap_device, host_ip, guest_ip, kernel_path, rootfs_path,
                    created_at, updated_at
             FROM vms WHERE name = ?1",
            params![name],
            row_to_vm_record,
        )
        .map_err(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => StateError::VmNotFoundByName(name.to_string()),
            other => StateError::Database(other),
        })
    }

    /// List all VMs.
    pub fn list_vms(&self) -> Result<Vec<VmRecord>, StateError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, name, state, pid, vcpu_count, mem_size_mib, vsock_cid,
                    tap_device, host_ip, guest_ip, kernel_path, rootfs_path,
                    created_at, updated_at
             FROM vms ORDER BY created_at",
        )?;

        let records = stmt
            .query_map([], row_to_vm_record)?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(records)
    }

    /// Update a VM's state.
    pub fn update_vm_state(&self, id: Uuid, state: &str) -> Result<(), StateError> {
        let conn = self.lock()?;
        let updated = conn.execute(
            "UPDATE vms SET state = ?1, updated_at = ?2 WHERE id = ?3",
            params![state, Utc::now().to_rfc3339(), id.to_string()],
        )?;
        if updated == 0 {
            return Err(StateError::VmNotFound(id));
        }
        Ok(())
    }

    /// Delete a VM record.
    pub fn delete_vm(&self, id: Uuid) -> Result<(), StateError> {
        let conn = self.lock()?;
        let deleted = conn.execute("DELETE FROM vms WHERE id = ?1", params![id.to_string()])?;
        if deleted == 0 {
            return Err(StateError::VmNotFound(id));
        }
        Ok(())
    }

    /// Allocate the next vsock CID.
    ///
    /// Reuses previously released CIDs (lowest first) before incrementing.
    /// CIDs start at 3 (0=hypervisor, 1=reserved, 2=host).
    pub fn allocate_cid(&self) -> Result<u32, StateError> {
        let mut conn = self.lock()?;
        let tx = conn.transaction()?;

        // Try freed CIDs first (lowest available)
        let freed: Option<u32> = tx
            .query_row(
                "SELECT cid FROM freed_cids ORDER BY cid LIMIT 1",
                [],
                |row| row.get(0),
            )
            .ok();

        let cid = if let Some(cid) = freed {
            tx.execute("DELETE FROM freed_cids WHERE cid = ?1", params![cid])?;
            debug!(cid, "reusing freed CID");
            cid
        } else {
            let cid: u32 = tx.query_row(
                "SELECT next_cid FROM cid_allocator WHERE id = 1",
                [],
                |row| row.get(0),
            )?;
            tx.execute(
                "UPDATE cid_allocator SET next_cid = next_cid + 1 WHERE id = 1",
                [],
            )?;
            debug!(cid, "allocated new CID");
            cid
        };

        tx.commit()?;
        Ok(cid)
    }

    /// Release a vsock CID back to the pool.
    ///
    /// Idempotent — releasing an already-freed CID is a no-op.
    pub fn release_cid(&self, cid: u32) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT OR IGNORE INTO freed_cids (cid) VALUES (?1)",
            params![cid],
        )?;
        debug!(cid, "released CID");
        Ok(())
    }

    // ── Port Forwards ─────────────────────────────────────────────────

    /// Insert a new port forward record.
    ///
    /// Returns `StateError::PortAlreadyForwarded` if the host port is already in use.
    pub fn insert_port_forward(&self, record: &PortForwardRecord) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute(
            "INSERT INTO port_forwards (vm_id, host_port, guest_port, protocol, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                record.vm_id.to_string(),
                record.host_port,
                record.guest_port,
                record.protocol,
                record.created_at.to_rfc3339(),
            ],
        )
        .map_err(|e| match &e {
            rusqlite::Error::SqliteFailure(err, _)
                if err.code == rusqlite::ErrorCode::ConstraintViolation =>
            {
                StateError::PortAlreadyForwarded(record.host_port)
            }
            _ => StateError::Database(e),
        })?;
        Ok(())
    }

    /// Delete a port forward by host port.
    pub fn delete_port_forward(&self, host_port: u16) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM port_forwards WHERE host_port = ?1",
            params![host_port],
        )?;
        Ok(())
    }

    /// List all port forwards for a VM, ordered by host port.
    pub fn list_port_forwards_for_vm(
        &self,
        vm_id: Uuid,
    ) -> Result<Vec<PortForwardRecord>, StateError> {
        let conn = self.lock()?;
        let mut stmt = conn.prepare(
            "SELECT id, vm_id, host_port, guest_port, protocol, created_at
             FROM port_forwards WHERE vm_id = ?1 ORDER BY host_port",
        )?;
        let records = stmt
            .query_map(params![vm_id.to_string()], |row| {
                let vm_id_str: String = row.get(1)?;
                let created_str: String = row.get(5)?;
                Ok(PortForwardRecord {
                    id: row.get(0)?,
                    vm_id: parse_uuid(&vm_id_str)?,
                    host_port: row.get::<_, u32>(2)? as u16,
                    guest_port: row.get::<_, u32>(3)? as u16,
                    protocol: row.get(4)?,
                    created_at: parse_datetime(&created_str)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(records)
    }

    /// Delete all port forwards for a VM.
    pub fn delete_port_forwards_for_vm(&self, vm_id: Uuid) -> Result<(), StateError> {
        let conn = self.lock()?;
        conn.execute(
            "DELETE FROM port_forwards WHERE vm_id = ?1",
            params![vm_id.to_string()],
        )?;
        Ok(())
    }
}

fn parse_uuid(s: &str) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(s).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn parse_datetime(s: &str) -> rusqlite::Result<DateTime<Utc>> {
    s.parse::<DateTime<Utc>>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e))
    })
}

fn row_to_vm_record(row: &rusqlite::Row<'_>) -> rusqlite::Result<VmRecord> {
    let id_str: String = row.get(0)?;
    let created_str: String = row.get(12)?;
    let updated_str: String = row.get(13)?;

    Ok(VmRecord {
        id: parse_uuid(&id_str)?,
        name: row.get(1)?,
        state: row.get(2)?,
        pid: row.get(3)?,
        vcpu_count: row.get(4)?,
        mem_size_mib: row.get(5)?,
        vsock_cid: row.get(6)?,
        tap_device: row.get(7)?,
        host_ip: row.get(8)?,
        guest_ip: row.get(9)?,
        kernel_path: row.get(10)?,
        rootfs_path: row.get(11)?,
        created_at: parse_datetime(&created_str)?,
        updated_at: parse_datetime(&updated_str)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_record(name: &str) -> VmRecord {
        VmRecord {
            id: Uuid::new_v4(),
            name: name.into(),
            state: "running".into(),
            pid: Some(1234),
            vcpu_count: 2,
            mem_size_mib: 256,
            vsock_cid: 3,
            tap_device: Some("tap0".into()),
            host_ip: Some("172.20.0.1".into()),
            guest_ip: Some("172.20.0.2".into()),
            kernel_path: "/boot/vmlinux".into(),
            rootfs_path: "/images/rootfs.ext4".into(),
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn insert_and_get() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("test-vm");
        store.insert_vm(&rec).unwrap();

        let fetched = store.get_vm(rec.id).unwrap();
        assert_eq!(fetched.name, "test-vm");
        assert_eq!(fetched.vcpu_count, 2);
    }

    #[test]
    fn get_by_name() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("my-vm");
        store.insert_vm(&rec).unwrap();

        let fetched = store.get_vm_by_name("my-vm").unwrap();
        assert_eq!(fetched.id, rec.id);
    }

    #[test]
    fn list_vms() {
        let store = StateStore::open_memory().unwrap();
        store.insert_vm(&make_record("vm-a")).unwrap();
        store.insert_vm(&make_record("vm-b")).unwrap();

        let list = store.list_vms().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn update_state() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("state-test");
        store.insert_vm(&rec).unwrap();

        store.update_vm_state(rec.id, "stopped").unwrap();
        let fetched = store.get_vm(rec.id).unwrap();
        assert_eq!(fetched.state, "stopped");
    }

    #[test]
    fn delete_vm() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("delete-me");
        store.insert_vm(&rec).unwrap();
        store.delete_vm(rec.id).unwrap();
        assert!(store.get_vm(rec.id).is_err());
    }

    #[test]
    fn cid_allocation() {
        let store = StateStore::open_memory().unwrap();
        assert_eq!(store.allocate_cid().unwrap(), 3);
        assert_eq!(store.allocate_cid().unwrap(), 4);
        assert_eq!(store.allocate_cid().unwrap(), 5);
    }

    #[test]
    fn vm_not_found() {
        let store = StateStore::open_memory().unwrap();
        let result = store.get_vm(Uuid::new_v4());
        assert!(matches!(result, Err(StateError::VmNotFound(_))));
    }

    #[test]
    fn roundtrip_preserves_timestamps() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("ts-test");
        let original_created = rec.created_at;
        store.insert_vm(&rec).unwrap();

        let fetched = store.get_vm(rec.id).unwrap();
        // RFC3339 roundtrip loses sub-nanosecond precision, compare seconds
        assert_eq!(fetched.created_at.timestamp(), original_created.timestamp());
    }

    #[test]
    fn vm_not_found_by_name() {
        let store = StateStore::open_memory().unwrap();
        let result = store.get_vm_by_name("nonexistent");
        assert!(matches!(result, Err(StateError::VmNotFoundByName(_))));
    }

    // ── CID Recycling ──────────────────────────────────────────────────

    #[test]
    fn cid_release_and_reuse() {
        let store = StateStore::open_memory().unwrap();
        let cid1 = store.allocate_cid().unwrap(); // 3
        let cid2 = store.allocate_cid().unwrap(); // 4
        assert_eq!(cid1, 3);
        assert_eq!(cid2, 4);

        // Release CID 3
        store.release_cid(cid1).unwrap();

        // Next allocation reuses CID 3
        let reused = store.allocate_cid().unwrap();
        assert_eq!(reused, 3);

        // Then fresh CID 5
        let fresh = store.allocate_cid().unwrap();
        assert_eq!(fresh, 5);
    }

    #[test]
    fn cid_release_reuses_lowest() {
        let store = StateStore::open_memory().unwrap();
        let cid3 = store.allocate_cid().unwrap(); // 3
        let _cid4 = store.allocate_cid().unwrap(); // 4
        let cid5 = store.allocate_cid().unwrap(); // 5

        // Release 5 then 3
        store.release_cid(cid5).unwrap();
        store.release_cid(cid3).unwrap();

        // Lowest freed (3) is reused first
        assert_eq!(store.allocate_cid().unwrap(), 3);
        assert_eq!(store.allocate_cid().unwrap(), 5);
        // Then fresh
        assert_eq!(store.allocate_cid().unwrap(), 6);
    }

    #[test]
    fn cid_double_release_is_idempotent() {
        let store = StateStore::open_memory().unwrap();
        let cid = store.allocate_cid().unwrap();

        store.release_cid(cid).unwrap();
        // Double release should not error
        store.release_cid(cid).unwrap();

        // Only allocated once on reuse
        assert_eq!(store.allocate_cid().unwrap(), cid);
        // Next is fresh, not cid again
        assert_eq!(store.allocate_cid().unwrap(), 4);
    }

    // ── Duplicate Name ─────────────────────────────────────────────────

    #[test]
    fn duplicate_name_rejected() {
        let store = StateStore::open_memory().unwrap();
        store.insert_vm(&make_record("dup")).unwrap();

        let mut dup = make_record("dup");
        dup.id = Uuid::new_v4(); // different ID, same name
        let err = store.insert_vm(&dup).unwrap_err();
        assert!(
            matches!(err, StateError::VmAlreadyExists(ref name) if name == "dup"),
            "expected VmAlreadyExists, got: {err}"
        );
    }

    // ── Update / Delete Edge Cases ─────────────────────────────────────

    #[test]
    fn update_state_nonexistent_vm() {
        let store = StateStore::open_memory().unwrap();
        let result = store.update_vm_state(Uuid::new_v4(), "stopped");
        assert!(matches!(result, Err(StateError::VmNotFound(_))));
    }

    #[test]
    fn delete_nonexistent_vm() {
        let store = StateStore::open_memory().unwrap();
        let result = store.delete_vm(Uuid::new_v4());
        assert!(matches!(result, Err(StateError::VmNotFound(_))));
    }

    #[test]
    fn update_state_updates_timestamp() {
        let store = StateStore::open_memory().unwrap();
        let rec = make_record("ts-update");
        store.insert_vm(&rec).unwrap();

        let before = store.get_vm(rec.id).unwrap().updated_at;
        std::thread::sleep(std::time::Duration::from_millis(10));
        store.update_vm_state(rec.id, "stopped").unwrap();
        let after = store.get_vm(rec.id).unwrap().updated_at;

        assert!(after >= before);
    }

    // ── File-backed Store ──────────────────────────────────────────────

    #[test]
    fn file_backed_persistence() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("test.db");

        let id;
        {
            let store = StateStore::open(&db_path).unwrap();
            let rec = make_record("persistent");
            id = rec.id;
            store.insert_vm(&rec).unwrap();
        }

        // Reopen and verify data persists
        let store = StateStore::open(&db_path).unwrap();
        let fetched = store.get_vm(id).unwrap();
        assert_eq!(fetched.name, "persistent");
    }

    // ── Port Forward CRUD ─────────────────────────────────────────────

    fn make_port_forward(vm_id: Uuid, host_port: u16, guest_port: u16) -> PortForwardRecord {
        PortForwardRecord {
            id: 0,
            vm_id,
            host_port,
            guest_port,
            protocol: "tcp".into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn insert_and_list_port_forwards() {
        let store = StateStore::open_memory().unwrap();
        let vm = make_record("pf-vm");
        store.insert_vm(&vm).unwrap();

        store
            .insert_port_forward(&make_port_forward(vm.id, 8080, 80))
            .unwrap();
        store
            .insert_port_forward(&make_port_forward(vm.id, 8443, 443))
            .unwrap();

        let forwards = store.list_port_forwards_for_vm(vm.id).unwrap();
        assert_eq!(forwards.len(), 2);
        assert_eq!(forwards[0].host_port, 8080);
        assert_eq!(forwards[0].guest_port, 80);
        assert_eq!(forwards[1].host_port, 8443);
        assert_eq!(forwards[1].guest_port, 443);
    }

    #[test]
    fn duplicate_host_port_rejected() {
        let store = StateStore::open_memory().unwrap();
        let vm = make_record("pf-dup");
        store.insert_vm(&vm).unwrap();

        store
            .insert_port_forward(&make_port_forward(vm.id, 8080, 80))
            .unwrap();

        let err = store
            .insert_port_forward(&make_port_forward(vm.id, 8080, 8080))
            .unwrap_err();
        assert!(
            matches!(err, StateError::PortAlreadyForwarded(8080)),
            "expected PortAlreadyForwarded(8080), got: {err}"
        );
    }

    #[test]
    fn delete_port_forward() {
        let store = StateStore::open_memory().unwrap();
        let vm = make_record("pf-del");
        store.insert_vm(&vm).unwrap();

        store
            .insert_port_forward(&make_port_forward(vm.id, 8080, 80))
            .unwrap();
        store
            .insert_port_forward(&make_port_forward(vm.id, 9090, 90))
            .unwrap();

        store.delete_port_forward(8080).unwrap();

        let forwards = store.list_port_forwards_for_vm(vm.id).unwrap();
        assert_eq!(forwards.len(), 1);
        assert_eq!(forwards[0].host_port, 9090);
    }

    #[test]
    fn delete_port_forwards_for_vm() {
        let store = StateStore::open_memory().unwrap();
        let vm1 = make_record("pf-vm1");
        let vm2 = make_record("pf-vm2");
        store.insert_vm(&vm1).unwrap();
        store.insert_vm(&vm2).unwrap();

        store
            .insert_port_forward(&make_port_forward(vm1.id, 8080, 80))
            .unwrap();
        store
            .insert_port_forward(&make_port_forward(vm1.id, 8443, 443))
            .unwrap();
        store
            .insert_port_forward(&make_port_forward(vm2.id, 9090, 90))
            .unwrap();

        store.delete_port_forwards_for_vm(vm1.id).unwrap();

        assert!(store.list_port_forwards_for_vm(vm1.id).unwrap().is_empty());
        assert_eq!(store.list_port_forwards_for_vm(vm2.id).unwrap().len(), 1);
    }

    #[test]
    fn cascade_delete_removes_port_forwards() {
        let store = StateStore::open_memory().unwrap();
        let vm = make_record("pf-cascade");
        store.insert_vm(&vm).unwrap();

        store
            .insert_port_forward(&make_port_forward(vm.id, 8080, 80))
            .unwrap();
        store
            .insert_port_forward(&make_port_forward(vm.id, 8443, 443))
            .unwrap();

        // Deleting the VM should cascade to port_forwards
        store.delete_vm(vm.id).unwrap();

        let forwards = store.list_port_forwards_for_vm(vm.id).unwrap();
        assert!(forwards.is_empty());
    }

    #[test]
    fn list_port_forwards_empty() {
        let store = StateStore::open_memory().unwrap();
        let vm = make_record("pf-empty");
        store.insert_vm(&vm).unwrap();

        let forwards = store.list_port_forwards_for_vm(vm.id).unwrap();
        assert!(forwards.is_empty());
    }
}
