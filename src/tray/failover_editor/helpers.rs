use super::*;

#[allow(unsafe_op_in_unsafe_fn)]
impl FailoverEditor {
    pub(super) fn route(&self, provider: &str) -> Option<&Route> {
        self.routes
            .iter()
            .find(|route| route.provider == provider && route.protocol == self.protocol)
    }

    pub(super) fn display(&self, provider: &str, ordered: bool) -> String {
        let Some(route) = self.route(provider) else {
            return provider.into();
        };
        if ordered {
            format!("{}  ·  {}", route.name, route.evidence_label())
        } else {
            format!(
                "{}  ·  {}  ·  {}",
                route.name,
                route.evidence_label(),
                route.host()
            )
        }
    }
}
