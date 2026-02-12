use std::path::Path;
use std::sync::Mutex;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum StateError {
    #[error("database error: {0}")]
    Database(#[from] rusqlite::Error),
    #[error("VM not found: {0}")]
    VmNotFound(Uuid),
    #[error("VM not found by name: {0}")]
    VmNotFoundByName(String),
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

            INSERT OR IGNORE INTO cid_allocator (id, next_cid) VALUES (1, 3);",
        )?;
        Ok(())
    }

    /// Insert a new VM record.
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
        )?;
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

    /// Update a VM's PID.
    pub fn update_vm_pid(&self, id: Uuid, pid: Option<u32>) -> Result<(), StateError> {
        let conn = self.lock()?;
        let updated = conn.execute(
            "UPDATE vms SET pid = ?1, updated_at = ?2 WHERE id = ?3",
            params![pid, Utc::now().to_rfc3339(), id.to_string()],
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
    pub fn allocate_cid(&self) -> Result<u32, StateError> {
        let conn = self.lock()?;
        let cid: u32 = conn.query_row(
            "SELECT next_cid FROM cid_allocator WHERE id = 1",
            [],
            |row| row.get(0),
        )?;
        conn.execute(
            "UPDATE cid_allocator SET next_cid = next_cid + 1 WHERE id = 1",
            [],
        )?;
        Ok(cid)
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
}
