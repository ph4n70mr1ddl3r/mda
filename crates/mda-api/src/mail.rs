//! Outbound email transport (PLAN §5.18): a pluggable [`MailSender`] boundary.
//!
//! The default [`SmtpMailSender`] speaks a minimal SMTP relay exchange
//! (EHLO → MAIL FROM → RCPT TO → DATA → QUIT) to an env-configured relay. When
//! no relay is configured it degrades to a safe [`NoopMailSender`] (the message
//! is still recorded in `sys_message` for audit + delivery retries). TLS,
//! SMTP-AUTH, and a full `lettre` impl are drop-ins behind the same trait.
//!
//! Config (env):
//! - `MDA_SMTP_HOST` / `MDA_SMTP_PORT` (default 25) — the relay endpoint.
//! - `MDA_SMTP_HELO` (default `mda.local`) — the EHLO name.
//! - `MDA_SMTP_FROM` (default `no-reply@mda.local`) — the envelope/`From`.

use async_trait::async_trait;
use mda_core::{Error, Result};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

/// An outbound email ready for the transport.
#[derive(Debug, Clone)]
pub struct OutgoingEmail {
    pub from: String,
    pub to: String,
    pub subject: String,
    pub body: String,
    pub content_type: String,
}

/// Pluggable email transport. `SmtpMailSender` ships now; a cloud-SES / `lettre`
/// impl is a drop-in (same trait).
#[async_trait]
pub trait MailSender: Send + Sync {
    async fn send(&self, msg: &OutgoingEmail) -> Result<()>;
}

/// A no-op sender — the default when no SMTP relay is configured. The message is
/// still recorded in `sys_message` by the email channel; this only skips the
/// network send.
pub struct NoopMailSender;

#[async_trait]
impl MailSender for NoopMailSender {
    async fn send(&self, msg: &OutgoingEmail) -> Result<()> {
        tracing::debug!(
            to = %msg.to,
            subject = %msg.subject,
            "email send skipped (no SMTP relay configured)"
        );
        Ok(())
    }
}

/// Resolve the default mail sender from the environment: a real relay when
/// `MDA_SMTP_HOST` is set, else [`NoopMailSender`].
pub fn sender_from_env() -> Arc<dyn MailSender> {
    SmtpMailSender::from_env()
}

/// A minimal SMTP relay client (plain TCP to a trusted relay). This is the
/// standard app → local-MTA hop (postfix, the docker MTA, a sidecar relay);
/// STARTTLS / SMTP-AUTH land behind the same trait for an internet-facing relay.
pub struct SmtpMailSender {
    pub host: String,
    pub port: u16,
    pub helo: String,
}

impl SmtpMailSender {
    /// Build the configured sender, or a [`NoopMailSender`] when unconfigured.
    pub fn from_env() -> Arc<dyn MailSender> {
        match std::env::var("MDA_SMTP_HOST")
            .ok()
            .filter(|s| !s.is_empty())
        {
            Some(host) => {
                let port: u16 = std::env::var("MDA_SMTP_PORT")
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(25);
                let helo =
                    std::env::var("MDA_SMTP_HELO").unwrap_or_else(|_| "mda.local".to_string());
                Arc::new(Self { host, port, helo })
            }
            None => Arc::new(NoopMailSender),
        }
    }
}

#[async_trait]
impl MailSender for SmtpMailSender {
    async fn send(&self, msg: &OutgoingEmail) -> Result<()> {
        let stream = TcpStream::connect((self.host.as_str(), self.port))
            .await
            .map_err(|e| {
                Error::internal(anyhow::anyhow!(
                    "smtp connect {}:{}: {e}",
                    self.host,
                    self.port
                ))
            })?;
        let (read, mut write) = stream.into_split();
        let mut reader = BufReader::new(read);

        // 220 greeting
        let line = read_reply(&mut reader).await?;
        require_code(&line, "220", &self.host)?;

        cmd(&mut write, &format!("EHLO {}\r\n", self.helo)).await?;
        // EHLO replies are multiline (250-… 250 ); drain to the terminal line.
        drain_multiline(&mut reader, "250", &self.host).await?;

        // Envelope addresses are scrubbed too — a CRLF in an address would
        // smuggle an extra SMTP command into the session (command injection).
        cmd(
            &mut write,
            &format!("MAIL FROM:<{}>\r\n", header_scrub(&msg.from)),
        )
        .await?;
        require_code(&read_reply(&mut reader).await?, "250", &self.host)?;

        cmd(
            &mut write,
            &format!("RCPT TO:<{}>\r\n", header_scrub(&msg.to)),
        )
        .await?;
        require_code(&read_reply(&mut reader).await?, "250", &self.host)?;

        cmd(&mut write, "DATA\r\n").await?;
        require_code(&read_reply(&mut reader).await?, "354", &self.host)?;

        // RFC 5321 4.5.2: dot-stuff every line beginning with '.', then close
        // the message body with a line containing only '.'.
        let stuffed = dot_stuff(&msg.body);
        // Header values are scrubbed of CR/LF/NUL: the subject is the
        // notification-type label and the content-type comes from stored
        // template metadata — both modeler-authored (§5.16 untrusted
        // metadata), and a bare CRLF in any of them would split/inject
        // headers into the DATA payload.
        let headers = format!(
            "From: {}\r\nTo: {}\r\nSubject: {}\r\nContent-Type: {}\r\n\r\n",
            header_scrub(&msg.from),
            header_scrub(&msg.to),
            header_scrub(&msg.subject),
            header_scrub(&msg.content_type)
        );
        let payload = format!("{headers}{stuffed}\r\n.\r\n");
        cmd(&mut write, payload.as_bytes()).await?;
        require_code(&read_reply(&mut reader).await?, "250", &self.host)?;

        let _ = cmd(&mut write, "QUIT\r\n").await;
        Ok(())
    }
}

async fn cmd<W: AsyncWriteExt + Unpin, B: AsRef<[u8]>>(w: &mut W, bytes: B) -> Result<()> {
    let bytes = bytes.as_ref();
    w.write_all(bytes)
        .await
        .map_err(|e| Error::internal(anyhow::anyhow!("smtp write failed: {e}")))?;
    w.flush()
        .await
        .map_err(|e| Error::internal(anyhow::anyhow!("smtp flush failed: {e}")))?;
    Ok(())
}

/// Read one (possibly multi-line) SMTP reply, returning its terminal line.
async fn read_reply<R: AsyncBufReadExt + Unpin>(reader: &mut R) -> Result<String> {
    loop {
        let mut line = String::new();
        let n = reader
            .read_line(&mut line)
            .await
            .map_err(|e| Error::internal(anyhow::anyhow!("smtp read failed: {e}")))?;
        if n == 0 {
            return Err(Error::internal(anyhow::anyhow!(
                "smtp: connection closed mid-reply"
            )));
        }
        let bytes = line.as_bytes();
        // A reply line "nnn-" is a continuation; "nnn " / a short line is terminal.
        if bytes.len() < 4 || bytes[3] != b'-' {
            return Ok(line);
        }
    }
}

/// Read replies until the terminal line's code matches `code` (multi-line drain).
async fn drain_multiline<R: AsyncBufReadExt + Unpin>(
    reader: &mut R,
    code: &str,
    host: &str,
) -> Result<()> {
    loop {
        let line = read_reply(reader).await?;
        if line.starts_with(code) {
            return Ok(());
        }
        // Any other positive reply keeps draining; an error code below is caught.
        if is_error_code(&line) {
            return Err(Error::internal(anyhow::anyhow!(
                "smtp {host}: unexpected reply `{}`",
                line.trim()
            )));
        }
    }
}

fn require_code(line: &str, code: &str, host: &str) -> Result<()> {
    if line.starts_with(code) {
        Ok(())
    } else {
        Err(Error::internal(anyhow::anyhow!(
            "smtp {host}: expected {code}, got `{}`",
            line.trim()
        )))
    }
}

fn is_error_code(line: &str) -> bool {
    let b = line.as_bytes();
    b.first()
        .is_some_and(|c| (*c as char).is_ascii_digit() && *c >= b'4')
}

/// RFC 5322 §2.2 / RFC 5321 §4.5.2: CR, LF, and NUL are forbidden in unfolded
/// header values and envelope commands. Scrub them (replace with a space) so a
/// modeler-authored subject / content-type / address can never split a header
/// or inject an SMTP command — the §5.16 untrusted-metadata rule applied to
/// the egress path.
fn header_scrub(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\r' | '\n' | '\0' => ' ',
            c => c,
        })
        .collect()
}

/// RFC 5322 4.5.2 dot-stuffing: a line beginning with '.' is prefixed with
/// another '.'. Operates on CRLF-split lines.
fn dot_stuff(body: &str) -> String {
    // Normalise lone LF to CRLF so the split is consistent.
    let normalised = body.replace("\r\n", "\n").replace('\n', "\r\n");
    normalised
        .split("\r\n")
        .map(|line| {
            if line.starts_with('.') {
                format!(".{line}")
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\r\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::net::TcpListener;

    #[test]
    fn dot_stuffing_escapes_leading_dot_lines() {
        assert_eq!(dot_stuff("hello"), "hello");
        // a line *beginning* with a dot is escaped.
        assert_eq!(dot_stuff(".secret"), "..secret");
        assert_eq!(dot_stuff("a\n.b\nc"), "a\r\n..b\r\nc");
        // a line that merely *ends* with a dot is unchanged.
        assert_eq!(dot_stuff("end."), "end.");
        // a lone-dot line (the caller adds the terminator separately) is escaped.
        assert_eq!(dot_stuff("."), "..");
    }

    #[test]
    fn header_values_never_carry_crlf() {
        // A modeler-authored subject carrying a CRLF must not be able to split
        // the Subject header (or inject an extra one) into the DATA payload.
        let evil = "Invoice\r\nBcc: attacker@evil.example";
        assert_eq!(header_scrub(evil), "Invoice  Bcc: attacker@evil.example");
        // LF-only and NUL are scrubbed too.
        assert_eq!(header_scrub("a\nb"), "a b");
        assert_eq!(header_scrub("a\0b"), "a b");
        // Clean values pass through byte-for-byte.
        assert_eq!(
            header_scrub("Invoice overdue — act now"),
            "Invoice overdue — act now"
        );
    }

    #[tokio::test]
    async fn noop_sender_succeeds() {
        NoopMailSender
            .send(&OutgoingEmail {
                from: "a@x".into(),
                to: "b@y".into(),
                subject: "s".into(),
                body: "hi".into(),
                content_type: "text/plain".into(),
            })
            .await
            .unwrap();
    }

    /// Read one CRLF-terminated line off a buf reader (panics on EOF).
    async fn read_line<R: AsyncBufReadExt + Unpin>(r: &mut R) -> String {
        let mut s = String::new();
        r.read_line(&mut s).await.unwrap();
        s
    }

    #[tokio::test]
    async fn smtp_sender_delivers_to_a_relay() {
        // A tiny mock SMTP relay that captures the DATA payload.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let captured: Arc<tokio::sync::Mutex<Option<String>>> =
            Arc::new(tokio::sync::Mutex::new(None));
        let cap = captured.clone();
        tokio::spawn(async move {
            let (sock, _) = listener.accept().await.unwrap();
            let (read, mut write) = sock.into_split();
            let mut reader = BufReader::new(read);
            write.write_all(b"220 mock ESMTP\r\n").await.unwrap();
            let _ehlo = read_line(&mut reader).await; // EHLO mda.local
            write
                .write_all(b"250-mock greets you\r\n250 8BITMIME\r\n")
                .await
                .unwrap();
            let mail = read_line(&mut reader).await;
            assert!(mail.starts_with("MAIL FROM:<from@mda.local>"), "{mail}");
            write.write_all(b"250 2.1.0 Ok\r\n").await.unwrap();
            let rcpt = read_line(&mut reader).await;
            assert!(rcpt.starts_with("RCPT TO:<to@mda.local>"), "{rcpt}");
            write.write_all(b"250 2.1.5 Ok\r\n").await.unwrap();
            let data_cmd = read_line(&mut reader).await;
            assert!(data_cmd.starts_with("DATA"), "{data_cmd}");
            write
                .write_all(b"354 End data with <CR><LF>.<CR><LF>\r\n")
                .await
                .unwrap();
            // read the message until the lone terminator dot line.
            let mut full = String::new();
            loop {
                let line = read_line(&mut reader).await;
                let terminal = line == ".\r\n";
                full.push_str(&line);
                if terminal {
                    break;
                }
            }
            // capture now, before the client-side drop after QUIT can race us.
            *cap.lock().await = Some(full.clone());
            write.write_all(b"250 2.0.0 Ok: queued\r\n").await.unwrap();
            // QUIT + 221 are best-effort: the client returns (and drops) once the
            // message is queued, so these may hit a closed socket.
            let _ = read_line(&mut reader).await;
            let _ = write.write_all(b"221 2.0.0 Bye\r\n").await;
        });

        let sender = SmtpMailSender {
            host: "127.0.0.1".to_string(),
            port,
            helo: "mda.local".to_string(),
        };
        sender
            .send(&OutgoingEmail {
                from: "from@mda.local".into(),
                to: "to@mda.local".into(),
                subject: "Hello".into(),
                body: "Line one\n.Line two".into(),
                content_type: "text/plain".into(),
            })
            .await
            .unwrap();

        let got = captured.lock().await.clone().unwrap();
        assert!(got.contains("From: from@mda.local"), "{got}");
        assert!(got.contains("To: to@mda.local"), "{got}");
        assert!(got.contains("Subject: Hello"), "{got}");
        assert!(got.contains("Line one"), "{got}");
        // RFC 5322 dot-stuffing: the leading-dot line was escaped to "..".
        assert!(got.contains("..Line two"), "dot-stuffed: {got}");
        assert!(got.ends_with(".\r\n"), "terminator dot: {got}");
    }
}
