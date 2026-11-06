//! The single counting channel/process fixture shared by authority and permission tests.
use super::*;

pub(super) struct CountingProcess {
    pub(super) polls: Arc<AtomicUsize>,
    pub(super) observation: Arc<AtomicUsize>,
}

#[async_trait]
impl ProcessHandle for CountingProcess {
    fn id(&self) -> &str {
        "counting-process"
    }

    async fn wait(&self) -> std::result::Result<ExitStatus, SandboxError> {
        loop {
            if let Some(status) = self.poll().await? {
                return Ok(status);
            }
            tokio::task::yield_now().await;
        }
    }

    async fn poll(&self) -> std::result::Result<Option<ExitStatus>, SandboxError> {
        self.polls.fetch_add(1, Ordering::SeqCst);
        match self.observation.load(Ordering::SeqCst) {
            0 => Ok(Some(ExitStatus {
                code: Some(0),
                signaled: false,
            })),
            1 => Ok(None),
            2 => Err(SandboxError::transport("transport unavailable")),
            _ => std::future::pending().await,
        }
    }

    async fn signal(&self, _signal: Signal) -> std::result::Result<(), SandboxError> {
        Ok(())
    }
}

pub(super) struct CountingSource {
    pub(super) opens: Arc<AtomicUsize>,
    pub(super) prompts: Arc<AtomicUsize>,
    pub(super) process_polls: Arc<AtomicUsize>,
    pub(super) observation: Arc<AtomicUsize>,
}

#[async_trait]
impl AgentChannelSource for CountingSource {
    fn check_tool_narrowing(
        &self,
        narrowing: &awaken_runtime_contract::permission::ToolCapabilityNarrowing,
    ) -> std::result::Result<(), OpenError> {
        crate::opaque_tool_narrowing(narrowing)
    }

    async fn open(
        &self,
        _activation: &RunActivation,
        _context: &RuntimeRunContext,
        _door: ChannelOpenDoor,
    ) -> std::result::Result<AgentSession, OpenError> {
        self.opens.fetch_add(1, Ordering::SeqCst);
        let (ours, mut theirs) = tokio::io::duplex(4096);
        let prompts = self.prompts.clone();
        tokio::spawn(async move {
            let mut prompt = String::new();
            let mut reader = BufReader::new(&mut theirs);
            if reader.read_line(&mut prompt).await.is_ok() && !prompt.is_empty() {
                prompts.fetch_add(1, Ordering::SeqCst);
            }
            let _ = theirs
                .write_all(b"{\"type\":\"message\",\"text\":\"done\"}\n")
                .await;
            let _ = theirs
                .write_all(b"{\"type\":\"turn_end\",\"reason\":\"natural_end\"}\n")
                .await;
            let _ = theirs.flush().await;
        });
        Ok(fixture_session(
            Box::new(ours),
            Arc::new(CountingProcess {
                polls: self.process_polls.clone(),
                observation: self.observation.clone(),
            }),
            Codec::Newline,
            None,
        ))
    }
}

pub(super) fn counting_executor() -> (
    AcpRunExecutor,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
) {
    let opens = Arc::new(AtomicUsize::new(0));
    let prompts = Arc::new(AtomicUsize::new(0));
    let process_polls = Arc::new(AtomicUsize::new(0));
    (
        AcpRunExecutor::new(Arc::new(CountingSource {
            opens: opens.clone(),
            prompts: prompts.clone(),
            process_polls: process_polls.clone(),
            observation: Arc::new(AtomicUsize::new(0)),
        })),
        opens,
        prompts,
        process_polls,
    )
}
