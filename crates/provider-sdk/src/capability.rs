//! Provider capabilities advertised in manifests.

/// A unit of upstream functionality a provider can expose.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Capability {
    /// Read account/authorization state.
    AccountState,
    /// Run the authorization flow for an account.
    Authorize,
    /// Read conversations and message history.
    ReadConversations,
    /// Send chat messages.
    SendMessage,
    /// Read the product catalog.
    ReadCatalog,
    /// Read and manage orders.
    ManageOrders,
    /// Push platform events (webhooks/SSE upstream).
    StreamEvents,
}

impl Capability {
    /// Canonical wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::AccountState => "accounts:read",
            Self::Authorize => "accounts:authorize",
            Self::ReadConversations => "conversations:read",
            Self::SendMessage => "conversations:write",
            Self::ReadCatalog => "catalog:read",
            Self::ManageOrders => "orders:manage",
            Self::StreamEvents => "events:stream",
        }
    }

    /// Parses a canonical wire name.
    pub fn parse(raw: &str) -> Option<Self> {
        [
            Self::AccountState,
            Self::Authorize,
            Self::ReadConversations,
            Self::SendMessage,
            Self::ReadCatalog,
            Self::ManageOrders,
            Self::StreamEvents,
        ]
        .into_iter()
        .find(|c| c.as_str() == raw)
    }
}

impl std::fmt::Display for Capability {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wire_names_roundtrip() {
        for cap in [
            Capability::AccountState,
            Capability::Authorize,
            Capability::ReadConversations,
            Capability::SendMessage,
            Capability::ReadCatalog,
            Capability::ManageOrders,
            Capability::StreamEvents,
        ] {
            assert_eq!(Capability::parse(cap.as_str()), Some(cap));
            assert_eq!(cap.to_string(), cap.as_str());
        }
        assert_eq!(Capability::parse("teleport"), None);
    }
}
