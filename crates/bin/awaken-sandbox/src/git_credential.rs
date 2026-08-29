//! One-shot Git credential helper for the closed Sandbox control channel.
//!
//! The helper owns no credential state. `get` asks the Session authority every
//! time; `store` and `erase` are deliberate no-ops so Git cannot extend a
//! capability lifetime or create a second cache inside the workload.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use awaken_sandbox_control::{
    CapabilityExpiresAtUnixMs, RepositoryGitCredentialQuery, SandboxControlRequest,
    SandboxControlResponse, read_frame, write_frame,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(unix)]
use tokio::net::UnixStream;
use zeroize::Zeroizing;

const MAX_GIT_CREDENTIAL_INPUT_BYTES: usize = 16 * 1024;
const GIT_CREDENTIAL_OPERATION_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Operation {
    Get,
    Store,
    Erase,
    Ignore,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum GitCredentialError {
    #[error("invalid helper invocation")]
    InvalidInvocation,
    #[error("invalid Git credential query")]
    InvalidQuery,
    #[error("Sandbox credential service is unavailable")]
    Unavailable,
    #[error("Repository credential is not authorized")]
    Denied,
}

fn parse_args(args: &[String]) -> Result<(PathBuf, Operation), GitCredentialError> {
    let [socket_flag, socket, operation] = args else {
        return Err(GitCredentialError::InvalidInvocation);
    };
    if socket_flag != "--socket"
        || socket != awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH
    {
        return Err(GitCredentialError::InvalidInvocation);
    }
    let operation = match operation.as_str() {
        "get" => Operation::Get,
        "store" => Operation::Store,
        "erase" => Operation::Erase,
        // Git's helper protocol is intentionally open ended. Capability
        // negotiation and future operations which this helper does not
        // implement are successful no-ops; importantly they do not read stdin
        // or dial the Sandbox control service.
        _ => Operation::Ignore,
    };
    Ok((PathBuf::from(socket), operation))
}

fn parse_query(bytes: &[u8]) -> Result<RepositoryGitCredentialQuery, GitCredentialError> {
    let text = std::str::from_utf8(bytes).map_err(|_| GitCredentialError::InvalidQuery)?;
    if text.contains('\0') {
        return Err(GitCredentialError::InvalidQuery);
    }
    let mut protocol = None;
    let mut host = None;
    let mut path = None;
    let mut terminated = false;
    for record in text.split_inclusive('\n') {
        let line = record.strip_suffix('\n').unwrap_or(record);
        let line = line.strip_suffix('\r').unwrap_or(line);
        if line.contains('\r') {
            return Err(GitCredentialError::InvalidQuery);
        }
        if line.is_empty() {
            terminated = true;
            continue;
        }
        if terminated {
            return Err(GitCredentialError::InvalidQuery);
        }
        let (key, value) = line
            .split_once('=')
            .ok_or(GitCredentialError::InvalidQuery)?;
        match key {
            "protocol" => set_authority_field(&mut protocol, value)?,
            "host" => set_authority_field(&mut host, value)?,
            "path" => set_authority_field(&mut path, value)?,
            // Git requires helpers to discard unrecognised attributes. Only
            // the unique protocol/host/path triple participates in authority;
            // extensions (including a future `url` record) cannot replace it.
            _ => {}
        }
    }
    let query = RepositoryGitCredentialQuery {
        protocol: protocol.ok_or(GitCredentialError::InvalidQuery)?,
        host: host.ok_or(GitCredentialError::InvalidQuery)?,
        path: path.ok_or(GitCredentialError::InvalidQuery)?,
    };
    query
        .validate()
        .map_err(|_| GitCredentialError::InvalidQuery)?;
    Ok(query)
}

fn set_authority_field(target: &mut Option<String>, value: &str) -> Result<(), GitCredentialError> {
    if target.replace(value.to_owned()).is_some() {
        return Err(GitCredentialError::InvalidQuery);
    }
    Ok(())
}

fn unix_now_ms() -> Result<u64, GitCredentialError> {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| GitCredentialError::Unavailable)?
        .as_millis();
    u64::try_from(millis).map_err(|_| GitCredentialError::Unavailable)
}

async fn get<R, W>(socket: &Path, input: &mut R, output: &mut W) -> Result<(), GitCredentialError>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    let bytes = read_credential_input(input).await?;
    let query = parse_query(&bytes)?;
    get_for_query(socket, query, output).await
}

#[cfg(unix)]
async fn get_for_query<W>(
    socket: &Path,
    query: RepositoryGitCredentialQuery,
    output: &mut W,
) -> Result<(), GitCredentialError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    let mut channel = UnixStream::connect(socket)
        .await
        .map_err(|_| GitCredentialError::Unavailable)?;
    write_frame(
        &mut channel,
        &SandboxControlRequest::RepositoryGitCredentialGet { query },
    )
    .await
    .map_err(|_| GitCredentialError::Unavailable)?;
    let response = read_frame::<_, SandboxControlResponse>(&mut channel)
        .await
        .map_err(|_| GitCredentialError::Unavailable)?;
    match response {
        SandboxControlResponse::RepositoryGitCredential {
            username,
            password,
            expires_at_unix_ms: CapabilityExpiresAtUnixMs(expires_at_unix_ms),
        } => {
            let mut rendered = Zeroizing::new(Vec::with_capacity(
                username.expose_secret().len() + password.expose_secret().len() + 21,
            ));
            rendered.extend_from_slice(b"username=");
            rendered.extend_from_slice(username.expose_secret().as_bytes());
            rendered.extend_from_slice(b"\npassword=");
            rendered.extend_from_slice(password.expose_secret().as_bytes());
            rendered.extend_from_slice(b"\n\n");

            // Sample liveness only after the authority response has been fully
            // decoded and the output is ready. Bound the write by the remaining
            // capability lifetime so a blocked stdout cannot emit stale material.
            let now_unix_ms = unix_now_ms()?;
            let remaining_ms = expires_at_unix_ms
                .checked_sub(now_unix_ms)
                .filter(|remaining| *remaining > 0)
                .ok_or(GitCredentialError::Unavailable)?;
            let output_deadline =
                Duration::from_millis(remaining_ms).min(GIT_CREDENTIAL_OPERATION_TIMEOUT);
            tokio::time::timeout(output_deadline, async {
                output.write_all(&rendered).await?;
                output.flush().await
            })
            .await
            .map_err(|_| GitCredentialError::Unavailable)?
            .map_err(|_| GitCredentialError::Unavailable)
        }
        SandboxControlResponse::Unavailable => Err(GitCredentialError::Unavailable),
        SandboxControlResponse::Denied => Err(GitCredentialError::Denied),
    }
}

#[cfg(not(unix))]
async fn get_for_query<W>(
    _socket: &Path,
    _query: RepositoryGitCredentialQuery,
    _output: &mut W,
) -> Result<(), GitCredentialError>
where
    W: AsyncWrite + Unpin + ?Sized,
{
    Err(GitCredentialError::Unavailable)
}

async fn read_credential_input<R>(input: &mut R) -> Result<Zeroizing<Vec<u8>>, GitCredentialError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let mut bytes = Zeroizing::new(Vec::with_capacity(1024));
    let mut line_start = 0;
    loop {
        let mut byte = [0_u8; 1];
        let read = input
            .read(&mut byte)
            .await
            .map_err(|_| GitCredentialError::InvalidQuery)?;
        if read == 0 {
            break;
        }
        bytes.push(byte[0]);
        if bytes.len() > MAX_GIT_CREDENTIAL_INPUT_BYTES {
            return Err(GitCredentialError::InvalidQuery);
        }
        if byte[0] == b'\n' {
            let line = &bytes[line_start..];
            line_start = bytes.len();
            if matches!(line, b"\n" | b"\r\n") {
                break;
            }
        }
    }
    Ok(bytes)
}

async fn consume_input<R>(input: &mut R) -> Result<(), GitCredentialError>
where
    R: AsyncRead + Unpin + ?Sized,
{
    let _bytes = read_credential_input(input).await?;
    Ok(())
}

async fn run_operation<R, W>(
    socket: &Path,
    operation: Operation,
    input: &mut R,
    output: &mut W,
) -> Result<(), GitCredentialError>
where
    R: AsyncRead + Unpin + ?Sized,
    W: AsyncWrite + Unpin + ?Sized,
{
    match operation {
        Operation::Store | Operation::Erase => consume_input(input).await,
        Operation::Get => get(socket, input, output).await,
        Operation::Ignore => Ok(()),
    }
}

async fn run_inner(args: &[String]) -> Result<(), GitCredentialError> {
    let (socket, operation) = parse_args(args)?;
    let mut input = tokio::io::stdin();
    let mut output = tokio::io::stdout();
    run_operation(&socket, operation, &mut input, &mut output).await
}

pub async fn run(args: &[String]) -> Result<(), GitCredentialError> {
    tokio::time::timeout(GIT_CREDENTIAL_OPERATION_TIMEOUT, run_inner(args))
        .await
        .map_err(|_| GitCredentialError::Unavailable)?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    async fn response_socket(
        response: SandboxControlResponse,
    ) -> (
        tempfile::TempDir,
        PathBuf,
        tokio::task::JoinHandle<SandboxControlRequest>,
    ) {
        let directory = tempfile::tempdir().expect("socket directory");
        let socket = directory.path().join("credential.sock");
        let listener = tokio::net::UnixListener::bind(&socket).expect("bind test service");
        let task = tokio::spawn(async move {
            let (mut channel, _) = listener.accept().await.expect("accept helper");
            let request = read_frame(&mut channel).await.expect("read helper request");
            write_frame(&mut channel, &response)
                .await
                .expect("write helper response");
            request
        });
        (directory, socket, task)
    }

    #[test]
    fn helper_query_accepts_git_extensions_without_weakening_authority() {
        /* R4/R7/R9 cause/effect decision table.
         * C1=exact protocol/host/path once; C2=standard or future non-authority fields;
         * C3=duplicate/alternate authority or malformed trailing record.
         * E1=one typed query; E2=ignore compatibility metadata; E3=deny
         * ambiguous target selection. Rules: R4 C1=>E1; R4 C1+C2=>E1+E2;
         * R9 C3=>E3.
         */
        assert!(parse_query(b"protocol=https\nhost=gateway.test\npath=git/repo\n\n").is_ok());
        assert!(
            parse_query(
                b"protocol=https\r\nhost=gateway.test\r\npath=git/repo\r\nusername=git\r\nwwwauth[]=Bearer realm=test\r\ncapability[]=authtype\r\n\r\n"
            )
            .is_ok()
        );
        assert!(parse_query(b"protocol=https\nhost=a\nhost=b\npath=git/repo\n").is_err());
        assert!(parse_query(b"protocol=https\nhost=a\npath=git/repo\npath=git/other\n").is_err());
        assert!(
            parse_query(
                b"protocol=https\nhost=a\npath=git/repo\nurl=https://other/repo\nfuture-key=future-value\n"
            )
            .is_ok(),
            "R4/R7 unknown attributes are discarded without becoming authority",
        );
        assert!(parse_query(b"protocol=https\nhost=a\npath=git/repo\n\npassword=late\n").is_err());
        assert!(parse_query(b"protocol=https\rhost=a\npath=git/repo\n").is_err());
        assert!(parse_query(b"protocol=https\nhost=a\npath=git/repo\0\n").is_err());
    }

    #[test]
    fn store_and_erase_are_explicit_stateless_operations() {
        /* R7 cause/effect decision table. C1=store or erase invocation;
         * C2=bounded credential-protocol input. E1=consume then discard the
         * zeroized buffer; E2=never open a control channel or retain state.
         * Rule: R7 C1+C2=>E1+E2.
         */
        let socket = awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH;
        assert_eq!(
            parse_args(&["--socket".into(), socket.into(), "store".into()])
                .unwrap()
                .1,
            Operation::Store,
        );
        assert_eq!(
            parse_args(&["--socket".into(), socket.into(), "erase".into()])
                .unwrap()
                .1,
            Operation::Erase,
        );
    }

    #[tokio::test]
    async fn capability_and_future_operations_return_without_reading_or_dialing() {
        /* Open-operation cause/effect decision table:
         * C1=Git `capability` or a future operation; C2=stdin writer remains
         * open; C3=the configured socket does not exist. E1=success without
         * waiting for input; E2=zero output; E3=no control-service dial.
         * Rules: R7 C1+C2+C3=>E1+E2+E3. This is forward-compatible by rule,
         * not by an operation-version allowlist.
         */
        let socket = awaken_sandbox_control::REPOSITORY_GIT_CREDENTIAL_SOCKET_PATH;
        for name in ["capability", "future-operation"] {
            let operation = parse_args(&["--socket".into(), socket.into(), name.into()])
                .expect("open Git operation")
                .1;
            assert_eq!(operation, Operation::Ignore, "R7 open operation");
            let (_writer, mut reader) = tokio::io::duplex(1);
            let mut output = Vec::new();
            tokio::time::timeout(
                Duration::from_millis(50),
                run_operation(
                    Path::new("/definitely/not/a/socket"),
                    operation,
                    &mut reader,
                    &mut output,
                ),
            )
            .await
            .expect("R7/E1 does not read stdin")
            .expect("R7/E1 succeeds");
            assert!(output.is_empty(), "R7/E2");
        }
    }

    #[tokio::test]
    async fn credential_input_stops_at_lf_or_crlf_frame_and_is_bounded() {
        /* R5/R7 cause/effect table:
         * C1=LF frame; C2=CRLF frame; C3=caller keeps stdin open after the
         * blank terminator; C4=input exceeds the fixed bound. E1=return one
         * complete frame without waiting for EOF; E2=preserve the exact wire;
         * E3=reject and drop the zeroizing buffer. Rules: R5 C1|C2=>E1+E2;
         * R5 C1+C3=>E1; R7 C4=>E3.
         */
        for expected in [
            b"protocol=https\nhost=gateway.test\npath=git/repo\n\n".as_slice(),
            b"protocol=https\r\nhost=gateway.test\r\npath=git/repo\r\n\r\n".as_slice(),
        ] {
            let (mut writer, mut reader) = tokio::io::duplex(256);
            writer.write_all(expected).await.unwrap();
            let observed = tokio::time::timeout(
                Duration::from_millis(50),
                read_credential_input(&mut reader),
            )
            .await
            .expect("blank line, not EOF, terminates the frame")
            .unwrap();
            assert_eq!(observed.as_slice(), expected, "R5/E1/E2");
        }

        let oversized = vec![b'x'; MAX_GIT_CREDENTIAL_INPUT_BYTES + 1];
        let mut oversized = oversized.as_slice();
        assert_eq!(
            read_credential_input(&mut oversized).await.unwrap_err(),
            GitCredentialError::InvalidQuery,
            "R7/E3",
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn real_control_wire_maps_success_denial_unavailable_and_expiry() {
        /* R4/R6/R10/R12 cause/effect table:
         * C1=standard Git query plus 401 negotiation fields; C2=fresh typed
         * capability; C3=Denied; C4=Unavailable; C5=expired at response time.
         * E1=exact username/password on stdout; E2=no credential output and a
         * distinct denied result; E3=no output and unavailable result. Rules:
         * R4 C1+C2=>E1; R6 C3=>E2; R10 C4=>E3; R12 C5=>E3.
         */
        let input = b"protocol=https\r\nhost=gateway.test\r\npath=git/repo\r\nusername=old\r\npassword=old\r\nwwwauth[]=Bearer realm=test\r\ncapability[]=authtype\r\nauthtype=Bearer\r\ncredential=old\r\nstate[]=retry\r\n\r\n";
        let live_expiry = unix_now_ms().unwrap() + 10_000;
        let (directory, socket, server) =
            response_socket(SandboxControlResponse::RepositoryGitCredential {
                username: awaken_sandbox_control::SandboxControlSecret::new("git-user").unwrap(),
                password: awaken_sandbox_control::SandboxControlSecret::new("virtual-token")
                    .unwrap(),
                expires_at_unix_ms: CapabilityExpiresAtUnixMs(live_expiry),
            })
            .await;
        let mut source = input.as_slice();
        let mut output = Vec::new();
        run_operation(&socket, Operation::Get, &mut source, &mut output)
            .await
            .expect("live credential");
        assert_eq!(
            output, b"username=git-user\npassword=virtual-token\n\n",
            "R4/E1"
        );
        let SandboxControlRequest::RepositoryGitCredentialGet { query } = server.await.unwrap();
        assert_eq!(query.host, "gateway.test", "R4 target");
        drop(directory);

        for (response, expected) in [
            (SandboxControlResponse::Denied, GitCredentialError::Denied),
            (
                SandboxControlResponse::Unavailable,
                GitCredentialError::Unavailable,
            ),
            (
                SandboxControlResponse::RepositoryGitCredential {
                    username: awaken_sandbox_control::SandboxControlSecret::new("git-user")
                        .unwrap(),
                    password: awaken_sandbox_control::SandboxControlSecret::new("expired").unwrap(),
                    expires_at_unix_ms: CapabilityExpiresAtUnixMs(
                        unix_now_ms().unwrap().saturating_sub(1),
                    ),
                },
                GitCredentialError::Unavailable,
            ),
        ] {
            let (_directory, socket, server) = response_socket(response).await;
            let mut source = input.as_slice();
            let mut output = Vec::new();
            assert_eq!(
                run_operation(&socket, Operation::Get, &mut source, &mut output)
                    .await
                    .unwrap_err(),
                expected,
            );
            assert!(output.is_empty(), "R6/R10/R12 no credential output");
            let _ = server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn stateless_operations_never_dial_and_input_wait_is_deadline_bounded() {
        /* R7/R13 cause/effect table:
         * C1=store/erase with a complete bounded frame; C2=nonexistent socket;
         * C3=stdin never terminates. E1=consume and zeroize without socket,
         * output, or state; E2=the total operation deadline interrupts the
         * wait. Rules: R7 C1+C2=>E1; R13 C3=>E2.
         */
        for operation in [Operation::Store, Operation::Erase] {
            let mut source = b"username=git\npassword=discard-me\n\n".as_slice();
            let mut output = Vec::new();
            run_operation(
                Path::new("/definitely/not/a/socket"),
                operation,
                &mut source,
                &mut output,
            )
            .await
            .expect("stateless no-op");
            assert!(output.is_empty(), "R7/E1");
        }

        let (_writer, mut reader) = tokio::io::duplex(1);
        let mut output = tokio::io::sink();
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            run_operation(
                Path::new("/definitely/not/a/socket"),
                Operation::Store,
                &mut reader,
                &mut output,
            ),
        )
        .await;
        assert!(
            result.is_err(),
            "R13 production wraps the same wait in one total deadline"
        );
    }
}
