//! One bounded private command codec; never serialize the domain planning type.
use super::invalid;
use awaken_provisioning_contract::MaterializedCommand;
use std::{ffi::OsStr, io, os::unix::ffi::OsStrExt, path::Path};
use tokio::io::{AsyncRead, AsyncReadExt};
use zeroize::Zeroizing;
pub(super) const MAX_FRAME: usize = 1024 * 1024;
const MAX_ITEMS: usize = 4096;

/// The helper's actual receive path. Validate the length before allocation;
/// retain no plaintext frame after decoding, including partial-read failures.
pub(super) async fn receive(
    input: &mut (impl AsyncRead + Unpin),
) -> io::Result<(u64, std::process::Command)> {
    let size = input.read_u32().await? as usize;
    if size == 0 || size > MAX_FRAME {
        return Err(invalid());
    }
    let mut frame = Zeroizing::new(vec![0; size]);
    input.read_exact(&mut frame).await?;
    parse(&frame)
}

// A bounded private OS handoff, intentionally NOT serde on MaterializedCommand:
// that domain/planning type must remain non-serializable. Decode borrows the
// zeroizing frame; only the final OS Command owns copies for exec. No Debug,
// persistence, log, or error includes any payload field.
pub(super) fn encode(
    command: &MaterializedCommand,
    cwd: &Path,
    deadline: u64,
) -> io::Result<Zeroizing<Vec<u8>>> {
    if !cwd.is_absolute() || command.argv.is_empty() || command.argv[0].is_empty() {
        return Err(invalid());
    }
    // Avoid realloc leaving earlier plaintext copies in the allocator.
    let mut frame = Zeroizing::new(Vec::with_capacity(MAX_FRAME));
    frame.extend_from_slice(b"AWSC2");
    frame.extend_from_slice(&deadline.to_be_bytes());
    field(&mut frame, cwd.as_os_str().as_bytes())?;
    count(&mut frame, command.argv.len())?;
    for arg in &command.argv {
        field(&mut frame, arg.as_bytes())?;
    }
    count(&mut frame, command.env.len())?;
    for env in &command.env {
        if env.name.is_empty() || env.name.contains('=') {
            return Err(invalid());
        }
        field(&mut frame, env.name.as_bytes())?;
        field(&mut frame, env.value.expose().as_bytes())?;
    }
    Ok(frame)
}

fn count(frame: &mut Vec<u8>, size: usize) -> io::Result<()> {
    if size > MAX_ITEMS || frame.len() + 4 > MAX_FRAME {
        return Err(invalid());
    }
    frame.extend_from_slice(&(size as u32).to_be_bytes());
    Ok(())
}

fn field(frame: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if bytes.contains(&0) || bytes.len() > MAX_FRAME || frame.len() + 4 + bytes.len() > MAX_FRAME {
        return Err(invalid());
    }
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(bytes);
    Ok(())
}

struct Input<'a>(&'a [u8]);

impl<'a> Input<'a> {
    fn take(&mut self, count: usize) -> io::Result<&'a [u8]> {
        if count > self.0.len() {
            return Err(invalid());
        }
        let (value, rest) = self.0.split_at(count);
        self.0 = rest;
        Ok(value)
    }

    fn u32(&mut self) -> io::Result<usize> {
        Ok(u32::from_be_bytes(self.take(4)?.try_into().map_err(|_| invalid())?) as usize)
    }

    fn u64(&mut self) -> io::Result<u64> {
        Ok(u64::from_be_bytes(
            self.take(8)?.try_into().map_err(|_| invalid())?,
        ))
    }

    fn field(&mut self) -> io::Result<&'a OsStr> {
        let size = self.u32()?;
        let value = self.take(size)?;
        if value.contains(&0) {
            return Err(invalid());
        }
        Ok(OsStr::from_bytes(value))
    }

    fn count(&mut self) -> io::Result<usize> {
        let size = self.u32()?;
        if size > MAX_ITEMS {
            return Err(invalid());
        }
        Ok(size)
    }
}

pub(super) fn parse(frame: &[u8]) -> io::Result<(u64, std::process::Command)> {
    if frame.len() > MAX_FRAME {
        return Err(invalid());
    }
    let mut input = Input(frame);
    if input.take(5)? != b"AWSC2" {
        return Err(invalid());
    }
    let deadline = input.u64()?;
    let cwd = Path::new(input.field()?);
    let argc = input.count()?;
    if !cwd.is_absolute() || argc == 0 {
        return Err(invalid());
    }
    let program = input.field()?;
    if program.is_empty() {
        return Err(invalid());
    }
    let mut command = std::process::Command::new(program);
    command.env_clear().current_dir(cwd);
    for _ in 1..argc {
        command.arg(input.field()?);
    }
    for _ in 0..input.count()? {
        let name = input.field()?;
        if name.is_empty() || name.as_bytes().contains(&b'=') {
            return Err(invalid());
        }
        command.env(name, input.field()?);
    }
    if !input.0.is_empty() {
        return Err(invalid());
    }
    Ok((deadline, command))
}

#[cfg(test)]
mod tests {
    use super::*;
    use awaken_provisioning_contract::{MaterializedEnvValue, MaterializedEnvVar};
    fn fixture() -> MaterializedCommand {
        let mut command = MaterializedCommand::new(["/bin/echo", "", "a b\n$(not-a-shell)"]);
        command.env.push(MaterializedEnvVar {
            name: "PRIVATE_VALUE".into(),
            value: MaterializedEnvValue::Secret("test-only-secret".to_owned().into()),
        });
        command
    }

    #[tokio::test]
    async fn helper_receive_rejects_bad_lengths_partial_frames_and_disconnect() {
        // W1 zero/oversized length with peer still open => immediate rejection,
        // not allocation or wait for attacker-selected bytes. W2 EOF in header
        // or payload => reject, never a command. W3 complete malformed payload
        // => reject. W4 valid exact frame => decode only that frame, preserving
        // the following JOINED/GO protocol bytes. Uses the real helper decoder.
        use tokio::io::AsyncWriteExt;
        for size in [0, MAX_FRAME as u32 + 1, u32::MAX] {
            let (mut peer, mut input) = tokio::io::duplex(4);
            peer.write_u32(size).await.unwrap();
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), receive(&mut input))
                    .await
                    .expect("invalid size must not wait for payload")
                    .is_err()
            );
        }
        let frame = encode(&fixture(), Path::new("/tmp"), 42).unwrap();
        let mut bytes = (frame.len() as u32).to_be_bytes().to_vec();
        bytes.extend_from_slice(&frame);
        for end in 0..bytes.len() {
            assert!(receive(&mut &bytes[..end]).await.is_err(), "W2 at {end}");
        }
        let mut invalid_bytes = bytes.clone();
        invalid_bytes[4] = b'X';
        assert!(receive(&mut invalid_bytes.as_slice()).await.is_err());
        bytes.push(99);
        let mut input = bytes.as_slice();
        assert_eq!(receive(&mut input).await.unwrap().0, 42);
        assert_eq!(input, &[99]);
    }

    #[test]
    fn private_handoff_roundtrips_exact_fields_and_rejects_malformed_input() {
        // Cause/effect decision table (no kernel prerequisites):
        // B1 valid absolute paths + argv/env => byte-exact cwd, args, env;
        // B2 truncation, trailing data, wrong version, NUL, bad count => reject;
        // B3 relative cwd, empty program, invalid env key, size overflow => reject
        // before any spawn/control write. Shell metacharacters are data (B1).
        // Physical cwd may contain non-UTF8 bytes on Linux, unlike planning text.
        let cwd = Path::new(OsStr::from_bytes(b"/tmp/space \xff"));
        let command = fixture();
        let frame = encode(&command, cwd, 42).unwrap();
        let (actual, decoded) = parse(&frame).unwrap();
        assert_eq!(actual, 42);
        assert_eq!(decoded.get_current_dir(), Some(cwd));
        assert_eq!(decoded.get_program(), OsStr::new("/bin/echo"));
        assert_eq!(
            decoded.get_args().collect::<Vec<_>>(),
            [OsStr::new(""), OsStr::new("a b\n$(not-a-shell)")]
        );
        assert_eq!(
            decoded.get_envs().collect::<Vec<_>>(),
            [(
                OsStr::new("PRIVATE_VALUE"),
                Some(OsStr::new("test-only-secret"))
            )]
        );
        for end in 0..frame.len() {
            assert!(parse(&frame[..end]).is_err(), "B2 truncated at {end}");
        }
        let mut malformed = frame.to_vec();
        malformed.push(1);
        assert!(parse(&malformed).is_err());
        malformed = frame.to_vec();
        malformed[0] = b'X';
        assert!(parse(&malformed).is_err());
        malformed = frame.to_vec();
        malformed[17] = 0;
        assert!(parse(&malformed).is_err());
        assert!(Input(&u32::MAX.to_be_bytes()).count().is_err());
        assert!(encode(&command, Path::new("relative"), 42).is_err());
        for argv in [
            vec![],
            vec!["".into()],
            vec!["nul\0argument".into()],
            vec!["x".repeat(MAX_FRAME)],
        ] {
            let command = MaterializedCommand::new(argv);
            assert!(encode(&command, cwd, 42).is_err());
        }
        for name in ["", "bad=key", "bad\0key"] {
            let mut command = fixture();
            command.env[0].name = name.into();
            assert!(encode(&command, cwd, 42).is_err());
        }
    }
}
