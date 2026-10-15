//! 永続ストア: registry.json / state/installations/<id>.json / managed-mcp.json
//! 実行状態・履歴はレシピとは別に保存する (仕様 §12)。

use crate::error::{Error, Result};
use crate::models::*;
use crate::paths::{self, Layout};
use chrono::Utc;

const MAX_EVENTS: usize = 200;

pub struct Store {
    pub layout: Layout,
}

impl Store {
    pub fn open(layout: Layout) -> Result<Self> {
        layout.ensure()?;
        std::fs::create_dir_all(layout.state_dir().join("installations"))?;
        Ok(Self { layout })
    }

    // ----- registry -----

    pub fn load_registry(&self) -> Result<Registry> {
        let p = self.layout.registry_file();
        if !p.exists() {
            return Ok(Registry {
                schema_version: SCHEMA_VERSION,
                installations: vec![],
            });
        }
        paths::read_json(&p)
    }

    pub fn save_registry(&self, reg: &Registry) -> Result<()> {
        paths::atomic_write_json(&self.layout.registry_file(), reg)
    }

    /// レジストリを排他更新。クロージャ内で installations を編集する。
    /// (レジストリ自体のロックは installations 個別ロックとは別に短命で取る)
    pub fn update_registry<R>(&self, f: impl FnOnce(&mut Registry) -> Result<R>) -> Result<R> {
        let _file_lock = self.registry_lock()?;
        let mut reg = self.load_registry()?;
        let r = f(&mut reg)?;
        self.save_registry(&reg)?;
        Ok(r)
    }

    fn registry_lock(&self) -> Result<std::fs::File> {
        use fs2::FileExt;
        std::fs::create_dir_all(self.layout.locks_dir())?;
        let p = self.layout.locks_dir().join("registry.lock");
        let f = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(p)?;
        f.lock_exclusive()?;
        Ok(f)
    }

    pub fn find_installation(&self, id_or_name: &str) -> Result<Installation> {
        let reg = self.load_registry()?;
        // id 完全一致 → 名前完全一致 → id 前方一致 → 名前(大小文字無視) の順
        if let Some(i) = reg.installations.iter().find(|i| i.id == id_or_name) {
            return Ok(i.clone());
        }
        if let Some(i) = reg.installations.iter().find(|i| i.name == id_or_name) {
            return Ok(i.clone());
        }
        let pref: Vec<_> = reg
            .installations
            .iter()
            .filter(|i| i.id.starts_with(id_or_name))
            .collect();
        if pref.len() == 1 {
            return Ok(pref[0].clone());
        }
        let lower = id_or_name.to_lowercase();
        let named: Vec<_> = reg
            .installations
            .iter()
            .filter(|i| i.name.to_lowercase() == lower)
            .collect();
        if named.len() == 1 {
            return Ok(named[0].clone());
        }
        Err(Error::NotFound(format!(
            "installation '{id_or_name}' (候補: {})",
            reg.installations
                .iter()
                .map(|i| i.name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        )))
    }

    /// レジストリへ追加または既存エントリへの利用元関連付け。
    /// 同一 install_root+environment の既存 installation があればそれを返す (仕様 §4.2)。
    pub fn upsert_installation(
        &self,
        inst: Installation,
    ) -> Result<(Installation, bool /* created */)> {
        self.update_registry(|reg| {
            if let Some(existing) = reg.installations.iter_mut().find(|i| i.id == inst.id) {
                // 利用元・レシピの差分をマージ
                for u in &inst.usages {
                    if !existing.usages.contains(u) {
                        existing.usages.push(u.clone());
                    }
                }
                existing.updated_at = Some(Utc::now());
                return Ok((existing.clone(), false));
            }
            reg.installations.push(inst.clone());
            Ok((inst, true))
        })
    }

    // ----- per-installation state -----

    pub fn load_state(&self, installation_id: &str) -> Result<InstallState> {
        let p = self.layout.install_state_file(installation_id);
        if !p.exists() {
            return Ok(InstallState::default());
        }
        paths::read_json(&p)
    }

    pub fn save_state(&self, installation_id: &str, st: &InstallState) -> Result<()> {
        let mut st = st.clone();
        st.events.truncate(MAX_EVENTS);
        paths::atomic_write_json(&self.layout.install_state_file(installation_id), &st)
    }

    pub fn update_state<R>(
        &self,
        installation_id: &str,
        f: impl FnOnce(&mut InstallState),
    ) -> Result<R>
    where
        R: Default,
    {
        let mut st = self.load_state(installation_id)?;
        f(&mut st);
        self.save_state(installation_id, &st)?;
        Ok(R::default())
    }

    pub fn push_event(&self, installation_id: &str, kind: &str, detail: &str) {
        if let Ok(mut st) = self.load_state(installation_id) {
            st.events.insert(
                0,
                StateEvent {
                    at: Utc::now(),
                    kind: kind.into(),
                    detail: detail.into(),
                },
            );
            st.events.truncate(MAX_EVENTS);
            let _ = self.save_state(installation_id, &st);
        }
    }

    // ----- managed map -----

    pub fn load_managed_map(&self) -> Result<ManagedMap> {
        let p = self.layout.managed_map_file();
        if !p.exists() {
            return Ok(ManagedMap {
                schema_version: SCHEMA_VERSION,
                entries: vec![],
            });
        }
        paths::read_json(&p)
    }

    pub fn save_managed_map(&self, m: &ManagedMap) -> Result<()> {
        paths::atomic_write_json(&self.layout.managed_map_file(), m)
    }

    // ----- generation meta -----

    pub fn load_genmeta(&self, installation_id: &str, gen: &str) -> Result<GenerationMeta> {
        paths::read_json(
            &self
                .layout
                .generation_dir(installation_id, gen)
                .join("genmeta.json"),
        )
    }

    /// 有効世代ポインタを読む。無効・不存在は None。
    pub fn active_generation(&self, installation_id: &str) -> Result<Option<String>> {
        let p = self.layout.active_pointer(installation_id);
        if !p.exists() {
            return Ok(None);
        }
        #[derive(serde::Deserialize)]
        struct Ptr {
            generation: String,
        }
        match paths::read_json::<Ptr>(&p) {
            Ok(ptr) => Ok(Some(ptr.generation)),
            Err(e) => {
                crate::dlogw!("active.json 読み取り失敗 {installation_id}: {e}");
                Ok(None)
            }
        }
    }

    /// 有効世代ポインタの原子的切替 (仕様 §6.1)
    pub fn set_active_generation(&self, installation_id: &str, gen: &str) -> Result<()> {
        #[derive(serde::Serialize)]
        struct Ptr<'a> {
            generation: &'a str,
            applied_at: chrono::DateTime<Utc>,
            pid: u32,
        }
        paths::atomic_write_json(
            &self.layout.active_pointer(installation_id),
            &Ptr {
                generation: gen,
                applied_at: Utc::now(),
                pid: std::process::id(),
            },
        )
    }
}

impl PartialEq for UsageRef {
    fn eq(&self, other: &Self) -> bool {
        use UsageRef::*;
        match (self, other) {
            (
                McpServer {
                    config_path: a,
                    server_name: b,
                    ..
                },
                McpServer {
                    config_path: x,
                    server_name: y,
                    ..
                },
            ) => crate::paths::path_key(a) == crate::paths::path_key(x) && b == y,
            (Cli { specifier: a, .. }, Cli { specifier: b, .. }) => a == b,
            _ => false,
        }
    }
}
impl Eq for UsageRef {}
