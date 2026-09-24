use std::{
    collections::{BTreeMap, VecDeque},
    sync::{Arc, Mutex},
};

#[derive(Clone, Default)]
pub struct Diagnostics(Arc<Mutex<VecDeque<BTreeMap<String, String>>>>);

impl Diagnostics {
    pub fn install(&self) -> tracing::subscriber::DefaultGuard {
        // 仅当前测试线程，不替换其他测试（尤其插件日志测试）的全局订阅者。
        tracing::subscriber::set_default(self.clone())
    }

    pub fn records(&self) -> Vec<BTreeMap<String, String>> {
        self.0.lock().unwrap().iter().cloned().collect()
    }
}

struct Fields(BTreeMap<String, String>);

impl tracing::field::Visit for Fields {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

impl tracing::Subscriber for Diagnostics {
    fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
        metadata.is_event()
            && metadata.level() <= &tracing::Level::WARN
            && metadata.target().starts_with("gateway_plugin_runtime::")
    }

    fn new_span(&self, _: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        tracing::span::Id::from_u64(1)
    }

    fn record(&self, _: &tracing::span::Id, _: &tracing::span::Record<'_>) {}
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}

    fn event(&self, event: &tracing::Event<'_>) {
        if !self.enabled(event.metadata()) {
            return;
        }
        let mut fields = Fields(BTreeMap::new());
        event.record(&mut fields);
        eprintln!("{}: {:?}", event.metadata().target(), fields.0);
        let mut records = self.0.lock().unwrap();
        // 压测和故障场景只保留最近的诊断，不积累无界日志。
        if records.len() == 128 {
            records.pop_front();
        }
        records.push_back(fields.0);
    }
}
