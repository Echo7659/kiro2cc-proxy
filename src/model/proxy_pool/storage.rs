use super::*;
use std::fs::{self, OpenOptions};
use std::io::Write;

#[derive(Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StoredPool {
    #[serde(default)]
    configured: bool,
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    proxies: Vec<ProxyRecord>,
}

pub(super) fn load(path: &Path) -> anyhow::Result<PoolState> {
    let content = match fs::read(path) {
        Ok(content) => content,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(PoolState::default());
        }
        Err(_) => anyhow::bail!("Cannot read proxy pool storage."),
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o600))
            .map_err(|_| anyhow::anyhow!("Cannot protect proxy pool storage."))?;
    }
    let mut stored: StoredPool = serde_json::from_slice(&content)
        .map_err(|_| anyhow::anyhow!("Invalid proxy pool storage format."))?;
    anyhow::ensure!(
        stored.proxies.len() <= MAX_PROXY_COUNT,
        "Proxy pool capacity limit exceeded."
    );
    let mut ids = std::collections::HashSet::new();
    let mut routes = std::collections::HashSet::new();
    for record in &mut stored.proxies {
        anyhow::ensure!(
            record.id > 0 && ids.insert(record.id),
            "Invalid proxy pool identifiers."
        );
        let parsed = parser::parse(&record.url)
            .map_err(|_| anyhow::anyhow!("Invalid stored proxy route."))?;
        record.url = parsed.url;
        if record.username.is_none() {
            record.username = parsed.username;
            record.password = parsed.password;
        }
        if record.username.is_some() {
            record.password.get_or_insert_with(String::new);
        }
        anyhow::ensure!(
            record.username.as_deref().is_none_or(|u| !u.is_empty()),
            "Invalid stored proxy authentication."
        );
        anyhow::ensure!(
            record.username.is_some() || record.password.is_none(),
            "Invalid stored proxy authentication."
        );
        anyhow::ensure!(
            routes.insert((
                record.url.clone(),
                record.username.clone(),
                record.password.clone()
            )),
            "Duplicate stored proxy routes."
        );
        // 健康快照不能跨进程当作当前可用证据；未到冷却期的故障仍保留。
        if record.status == ProxyStatus::Healthy {
            record.status = ProxyStatus::Unknown;
        }
        stored.next_id = stored.next_id.max(record.id);
    }
    Ok(PoolState {
        configured: stored.configured || !stored.proxies.is_empty(),
        next_id: stored.next_id,
        entries: stored
            .proxies
            .into_iter()
            .map(|record| Entry {
                record,
                checking: None,
            })
            .collect(),
    })
}

pub(super) fn save(path: &Path, state: &PoolState) -> anyhow::Result<()> {
    let stored = StoredPool {
        configured: state.configured,
        next_id: state.next_id,
        proxies: state.entries.iter().map(|e| e.record.clone()).collect(),
    };
    let content = serde_json::to_vec_pretty(&stored)
        .map_err(|_| anyhow::anyhow!("Cannot serialize proxy pool storage."))?;
    secure_atomic_write(path, &content)
        .map_err(|_| anyhow::anyhow!("Cannot persist proxy pool storage."))
}

fn secure_atomic_write(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let directory = path.parent().unwrap_or_else(|| Path::new("."));
    fs::create_dir_all(directory)?;
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let temporary = directory.join(format!("{name}.tmp-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, path)?;
        if let Ok(dir) = fs::File::open(directory) {
            dir.sync_all()?;
        }
        Ok(())
    })();
    let _ = fs::remove_file(temporary);
    result
}
