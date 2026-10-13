//! The profile's context window is a ceiling: a server that reports a
//! smaller per-request window lowers it for the session, nothing raises it.
use builder::agent::{Agent, ApprovalMode, ContextFit};
use builder_core::{config::Profile, protocol::Message};
use builder_provider::{Event, Provider};
use builder_tools::Workspace;
use serde_json::Value;

struct Reports(Option<usize>);

impl Provider for Reports {
    async fn context_capacity(&self) -> Option<usize> {
        self.0
    }
    async fn complete(
        &self,
        _messages: &[Message],
        _tools: &[Value],
        _emit: &mut dyn FnMut(Event),
    ) -> anyhow::Result<Message> {
        unreachable!("fitting the context never generates")
    }
}

fn agent(server: Option<usize>, context_tokens: usize, max_output_tokens: usize) -> Agent<Reports> {
    let dir = tempfile::tempdir().unwrap();
    Agent {
        provider: Reports(server),
        memory: None,
        profile: Profile {
            context_tokens,
            max_output_tokens,
            ..Profile::default()
        },
        workspace: Workspace::new(dir.path()).unwrap(),
        session: "test".into(),
        approval: ApprovalMode::ReadOnly,
        max_rounds: None,
    }
}

#[tokio::test]
async fn a_smaller_server_window_lowers_the_profile() {
    let mut agent = agent(Some(131_072), 163_840, 8192);
    assert_eq!(
        agent.fit_context().await,
        Some(ContextFit {
            configured: 163_840,
            server: 131_072,
            max_output_tokens: None,
        })
    );
    assert_eq!(agent.profile.context_tokens, 131_072);
    assert_eq!(agent.profile.max_output_tokens, 8192);
}

#[tokio::test]
async fn the_output_budget_shrinks_only_when_the_window_no_longer_covers_it() {
    let mut agent = agent(Some(8192), 32_768, 8192);
    let fit = agent.fit_context().await.unwrap();
    assert_eq!(fit.max_output_tokens, Some(2048));
    assert_eq!(agent.profile.context_tokens, 8192);
    assert_eq!(agent.profile.max_output_tokens, 2048);
    assert!(agent.profile.context_tokens - agent.profile.max_output_tokens > 1024);
}

#[tokio::test]
async fn nothing_raises_the_configured_window() {
    for (server, configured) in [
        (None, 32_768),
        (Some(262_144), 32_768),
        (Some(32_768), 32_768),
    ] {
        let mut agent = agent(server, configured, 4096);
        assert_eq!(agent.fit_context().await, None, "{server:?}");
        assert_eq!(agent.profile.context_tokens, configured);
    }
}

#[tokio::test]
async fn an_implausibly_small_report_is_ignored() {
    let mut agent = agent(Some(512), 32_768, 4096);
    assert_eq!(agent.fit_context().await, None);
    assert_eq!(agent.profile.context_tokens, 32_768);
}
