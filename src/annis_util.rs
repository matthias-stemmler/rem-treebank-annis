use std::ops::Deref;

use tempfile::TempDir;

pub(crate) struct TempStorage {
    storage: graphannis::CorpusStorage,
    _db_dir: TempDir,
}

impl TempStorage {
    pub(crate) fn new() -> anyhow::Result<Self> {
        let db_dir = TempDir::new()?;
        let storage = graphannis::CorpusStorage::with_auto_cache_size(db_dir.path(), true)?;

        Ok(Self {
            storage,
            _db_dir: db_dir,
        })
    }
}

impl Deref for TempStorage {
    type Target = graphannis::CorpusStorage;

    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}
