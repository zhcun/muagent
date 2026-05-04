//! AdapterBundle:host 构造,传给 tools。冻结后不变。

use std::sync::Arc;

use super::fs::FileSystem;
use super::proc_exec::ProcessExec;

#[derive(Clone)]
pub struct AdapterBundle {
    pub fs: Arc<dyn FileSystem>,
    pub proc: Option<Arc<dyn ProcessExec>>,
}

impl AdapterBundle {
    pub fn builder() -> AdapterBundleBuilder {
        AdapterBundleBuilder::default()
    }
}

#[derive(Default)]
pub struct AdapterBundleBuilder {
    fs: Option<Arc<dyn FileSystem>>,
    proc: Option<Arc<dyn ProcessExec>>,
}

impl AdapterBundleBuilder {
    pub fn fs(mut self, fs: Arc<dyn FileSystem>) -> Self {
        self.fs = Some(fs);
        self
    }
    pub fn proc(mut self, p: Arc<dyn ProcessExec>) -> Self {
        self.proc = Some(p);
        self
    }

    pub fn build(self) -> Result<AdapterBundle, &'static str> {
        Ok(AdapterBundle {
            fs: self.fs.ok_or("fs required")?,
            proc: self.proc,
        })
    }
}
