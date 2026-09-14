//! Intentional, single-pane collaboration input.
use super::*;

const INPUT_IDLE_MS: u64 = 750;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PaneWriteSubmit {
    #[default]
    Auto,
    Enter,
    None,
}

impl ControlPlane {
    pub(super) fn intentional_pane_write(
        &mut self,
        client_raw: u64,
        pane_raw: u64,
        expected_child_pid: u32,
        data: String,
        submit: PaneWriteSubmit,
    ) -> Result<ControlResponseData, ControlError> {
        let client = self.require_active_client(client_raw)?;
        let pane = pane_id(pane_raw)?;
        self.require_client_space(client, pane)?;
        // Always check the ledger, including requests from the lease holder.
        self.domain
            .allow_write(pane, client)
            .map_err(control_domain_error)?;
        let live_pid = self.live.as_ref().and_then(|live| live.child_pid(pane_raw));
        if expected_child_pid == 0 || live_pid != Some(expected_child_pid) {
            return Err(ControlError::new(
                ControlErrorCode::StaleId,
                "pane child changed or exited; rediscover the target",
            ));
        }
        let ledger = self.pane_ledger(pane_raw);
        if ledger.dirty_input {
            return Err(ControlError::new(
                ControlErrorCode::InputDirty,
                "pane has unsubmitted input; no text was written",
            ));
        }
        let now = now_unix_ms();
        if ledger
            .last_input_at_ms
            .is_some_and(|at| now.saturating_sub(at) < INPUT_IDLE_MS)
            || self.output_bytes_in_quiet_window(pane_raw, now) >= MAIL_INJECT_BUSY_BYTES
        {
            return Err(ControlError::new(
                ControlErrorCode::InputBusy,
                "pane is typing or streaming output; retry when idle",
            ));
        }
        let chunks = pane_write_chunks(&data, submit, self.foreground_agent_for(pane_raw))?;
        let total_bytes = chunks.iter().map(Vec::len).sum();
        let (nbytes, error) = queue_chunks(&chunks, |chunk| {
            self.live
                .as_ref()
                .ok_or(LiveWriteError::Disconnected)
                .and_then(|live| live.write(pane_raw, chunk))
        });
        if nbytes == 0 {
            if let Some(error) = error {
                return Err(error);
            }
        }
        let complete = error.is_none();
        self.write_ledger.insert(
            pane_raw,
            (now_unix_ms(), complete && submit != PaneWriteSubmit::None),
        );
        self.last_input_at_ms.insert(pane_raw, now_unix_ms());
        self.pane_write_receipts.insert(
            pane_raw,
            PaneWriteReceipt {
                child_pid: expected_child_pid,
                queued_at_ms: now_unix_ms(),
                nbytes,
                total_bytes,
                complete,
                submit,
            },
        );
        Ok(ControlResponseData::PaneWriteResult {
            pane_id: pane_raw,
            child_pid: expected_child_pid,
            nbytes,
            total_bytes,
            complete,
            submit,
            error,
        })
    }
}

fn pane_write_chunks(
    data: &str,
    submit: PaneWriteSubmit,
    agent: crate::InjectAgent,
) -> Result<Vec<Vec<u8>>, ControlError> {
    if data.is_empty()
        || data.len() > MAX_SPAWN_BYTES - 32
        || data
            .chars()
            .any(|c| c.is_control() && c != '\n' && c != '\t')
    {
        return Err(ControlError::new(
            ControlErrorCode::InvalidRequest,
            "text must contain 1..65504 UTF-8 bytes and no controls except tab and newline",
        ));
    }
    match submit {
        PaneWriteSubmit::None => Ok(vec![data.as_bytes().to_vec()]),
        PaneWriteSubmit::Enter => Ok(vec![format!("{data}\r").into_bytes()]),
        PaneWriteSubmit::Auto => {
            if agent == crate::InjectAgent::Unknown {
                return Err(ControlError::new(
                    ControlErrorCode::InvalidRequest,
                    "no supported foreground agent; select submit enter or none explicitly",
                ));
            }
            // Paste the body as text, then submit using the existing guest
            // adapter. Mail's fixed-token injection remains unchanged.
            let body = format!("\x1b[200~{data}\x1b[201~").into_bytes();
            let submit = match agent {
                crate::InjectAgent::Cursor => vec![crate::inject_submit::CURSOR_SUBMIT.to_vec()],
                crate::InjectAgent::Codex => vec![vec![b'\r'], vec![b'\r']],
                _ => vec![vec![b'\r']],
            };
            Ok(std::iter::once(body).chain(submit).collect())
        }
    }
}

fn pane_write_error(error: LiveWriteError) -> ControlError {
    match error {
        LiveWriteError::Backpressure => {
            ControlError::new(ControlErrorCode::Backpressure, "pane input queue is full")
        }
        LiveWriteError::UnknownPane | LiveWriteError::Disconnected => ControlError::new(
            ControlErrorCode::InputRouteUnavailable,
            "pane input route is unavailable",
        ),
    }
}

// Queue each chunk once. Preserve accepted-byte accounting on backpressure.
fn queue_chunks(
    chunks: &[Vec<u8>],
    mut write: impl FnMut(Vec<u8>) -> Result<(), LiveWriteError>,
) -> (usize, Option<ControlError>) {
    let mut nbytes = 0;
    for (index, chunk) in chunks.iter().enumerate() {
        if let Err(error) = write(chunk.clone()) {
            return (nbytes, Some(pane_write_error(error)));
        }
        nbytes += chunk.len();
        if index + 1 < chunks.len() {
            thread::sleep(Duration::from_millis(80));
        }
    }
    (nbytes, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_write_partial_never_queues_submit_after_failure() {
        let chunks = vec![b"body".to_vec(), b"submit".to_vec(), b"later".to_vec()];
        let mut calls = 0;
        let (bytes, error) = queue_chunks(&chunks, |_| {
            calls += 1;
            if calls == 2 {
                Err(LiveWriteError::Backpressure)
            } else {
                Ok(())
            }
        });
        assert_eq!((bytes, calls), (4, 2));
        assert_eq!(error.unwrap().code, ControlErrorCode::Backpressure);
        for cause in [LiveWriteError::UnknownPane, LiveWriteError::Disconnected] {
            assert_eq!(
                pane_write_error(cause).code,
                ControlErrorCode::InputRouteUnavailable
            );
        }
    }

    #[test]
    fn pane_write_guest_submit_keeps_body_inside_one_paste() {
        use crate::InjectAgent::*;
        for (agent, terminators) in [
            (Grok, vec![b"\r".to_vec()]),
            (Claude, vec![b"\r".to_vec()]),
            (Kiro, vec![b"\r".to_vec()]),
            (Codex, vec![b"\r".to_vec(), b"\r".to_vec()]),
            (Cursor, vec![crate::inject_submit::CURSOR_SUBMIT.to_vec()]),
        ] {
            let chunks = pane_write_chunks("one\ntwo\tλ", PaneWriteSubmit::Auto, agent).unwrap();
            assert_eq!(chunks[0], "\x1b[200~one\ntwo\tλ\x1b[201~".as_bytes());
            assert_eq!(chunks[1..], terminators);
        }
        assert!(pane_write_chunks("hello", PaneWriteSubmit::Auto, Unknown).is_err());
        for bad in [
            "",
            "escape\x1b[201~",
            "submit\r",
            "nul\0",
            &"x".repeat(65505),
        ] {
            assert!(pane_write_chunks(bad, PaneWriteSubmit::Enter, Grok).is_err());
        }
        assert_eq!(
            pane_write_chunks("literal\\n", PaneWriteSubmit::None, Unknown).unwrap(),
            vec![b"literal\\n".to_vec()]
        );
        assert_eq!(
            pane_write_chunks("λ", PaneWriteSubmit::Enter, Unknown).unwrap(),
            vec!["λ\r".as_bytes()]
        );
    }
}
