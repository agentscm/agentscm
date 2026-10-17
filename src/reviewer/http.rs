use std::net::SocketAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::contracts::*;
use super::wire::decode_reply;
use crate::{Error, Result};

/// One explicit loopback HTTP service. Locality does not prove server identity,
/// absence of forwarding, hard resource limits or useful real-model execution.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LocalHttpConfig {
    pub address: SocketAddr,
    pub model: String,
}

impl LocalHttpConfig {
    pub fn validate(&self) -> Result<()> {
        validate_identifier(&self.model)?;
        if !self.address.ip().is_loopback() || self.address.port() == 0 {
            return Err(Error::Invalid("model HTTP endpoint must be an explicit numeric loopback address with a nonzero port".into()));
        }
        Ok(())
    }

    pub fn scope_sha256(&self) -> Result<String> {
        self.validate()?;
        domain_digest("agentscm.local-chat-completions-http/v1", self)
    }
}

pub struct LocalHttpTransport {
    config: LocalHttpConfig,
    scope: String,
    client: reqwest::Client,
}

impl LocalHttpTransport {
    pub fn new(config: LocalHttpConfig) -> Result<Self> {
        let scope = config.scope_sha256()?;
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .http1_only()
            .pool_max_idle_per_host(0)
            .connect_timeout(Duration::from_secs(2))
            .timeout(Duration::from_secs(60))
            .build()
            .map_err(|_| Error::Invalid("could not build finite model HTTP client".into()))?;
        Ok(Self {
            config,
            scope,
            client,
        })
    }

    async fn invoke_inner(
        &self,
        call: &PreparedModelCall,
        cancel: &ReviewCancellation,
    ) -> ModelCallObservation {
        let mut observation = ModelCallObservation {
            state: CallState::NotStarted,
            basis: TransportBasis::LocalHttpUnqualified,
            reply: None,
            failure: None,
            http_status: None,
            captured_response_bytes: 0,
            captured_response_sha256: hash_bytes(&[]),
            captured_response: Vec::new(),
            response_capture_complete: false,
        };
        if cancel.is_cancelled() {
            observation.failure = Some(ModelFailure::Cancelled);
            return observation;
        }
        if call.expected_model != self.config.model
            || call.body.len() > MAX_REQUEST_BYTES
            || call.max_response_bytes > MAX_RESPONSE_BYTES
            || call.timeout_ms == 0
            || call.timeout_ms > 60_000
        {
            observation.failure = Some(ModelFailure::InvalidResponse);
            return observation;
        }
        let url = format!("http://{}/v1/chat/completions", self.config.address);
        // Reserve before invoke. Once send is polled, a connection failure cannot
        // establish that the server saw or processed no bytes.
        observation.state = CallState::Uncertain;
        let deadline =
            tokio::time::Instant::now() + Duration::from_millis(u64::from(call.timeout_ms));
        let mut captured = Vec::new();
        let exchange = async {
            let mut response = self
                .client
                .post(url)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .body(call.body.clone())
                .send()
                .await
                .map_err(|_| ModelFailure::Connection)?;
            observation.http_status = Some(response.status().as_u16());
            if response.status() != reqwest::StatusCode::OK {
                return Err(ModelFailure::HttpStatus);
            }
            if response
                .content_length()
                .is_some_and(|len| len > call.max_response_bytes as u64)
            {
                return Err(ModelFailure::ResponseLimit);
            }
            if response
                .headers()
                .get(reqwest::header::CONTENT_ENCODING)
                .is_some()
            {
                return Err(ModelFailure::InvalidResponse);
            }
            let content_type = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("");
            if content_type.split(';').next().map(str::trim) != Some("application/json") {
                return Err(ModelFailure::InvalidResponse);
            }
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|_| ModelFailure::Connection)?
            {
                let remaining = call.max_response_bytes.saturating_sub(captured.len());
                captured.extend_from_slice(&chunk[..chunk.len().min(remaining)]);
                if chunk.len() > remaining {
                    return Err(ModelFailure::ResponseLimit);
                }
            }
            observation.response_capture_complete = true;
            decode_reply(&captured, &call.expected_model, call.max_output_tokens)
        };
        let cancelled = async {
            loop {
                if cancel.is_cancelled() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        };
        let result = tokio::select! {
            biased;
            _ = cancelled => Err(ModelFailure::Cancelled),
            _ = tokio::time::sleep_until(deadline) => Err(ModelFailure::Deadline),
            result = exchange => result,
        };
        observation.captured_response_bytes = captured.len() as u64;
        observation.captured_response_sha256 = hash_bytes(&captured);
        observation.captured_response = captured;
        match result {
            Ok(reply) => {
                observation.state = CallState::Acknowledged;
                observation.reply = Some(reply);
            }
            Err(failure) => observation.failure = Some(failure),
        }
        observation
    }
}

impl ModelTransport for LocalHttpTransport {
    fn scope_sha256(&self) -> String {
        self.scope.clone()
    }
    fn basis(&self) -> TransportBasis {
        TransportBasis::LocalHttpUnqualified
    }

    fn invoke<'a>(
        &'a self,
        call: &'a PreparedModelCall,
        cancel: &'a ReviewCancellation,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ModelCallObservation> + Send + 'a>>
    {
        Box::pin(self.invoke_inner(call, cancel))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::thread;
    use std::time::Instant;

    use crate::checks::CheckSubject;
    use crate::context::ChangeId;

    fn request(stream: &mut TcpStream) -> Vec<u8> {
        // Accepted sockets can inherit the listener's nonblocking mode on macOS.
        // The fixture uses bounded blocking I/O after the bounded accept loop.
        stream.set_nonblocking(false).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut bytes = Vec::new();
        let mut byte = [0];
        while !bytes.ends_with(b"\r\n\r\n") {
            assert!(bytes.len() < 8192);
            stream.read_exact(&mut byte).unwrap();
            bytes.push(byte[0]);
        }
        let headers = std::str::from_utf8(&bytes).unwrap();
        assert!(headers.starts_with("POST /v1/chat/completions HTTP/1.1\r\n"));
        assert!(!headers.to_ascii_lowercase().contains("authorization:"));
        assert!(!headers.to_ascii_lowercase().contains("cookie:"));
        let length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(str::trim)
                    .map(str::to_owned)
            })
            .unwrap()
            .parse::<usize>()
            .unwrap();
        assert!(length <= 4096);
        let offset = bytes.len();
        bytes.resize(offset + length, 0);
        stream.read_exact(&mut bytes[offset..]).unwrap();
        bytes
    }

    fn call(timeout_ms: u32, max_response_bytes: usize) -> PreparedModelCall {
        PreparedModelCall {
            body: br#"{"model":"fixture-v1","tools":[],"tool_choice":"none"}"#.to_vec(),
            expected_model: "fixture-v1".into(),
            max_response_bytes,
            max_output_tokens: 128,
            timeout_ms,
        }
    }

    fn valid_body() -> Vec<u8> {
        response_body(
            "fixture-v1",
            serde_json::json!({"action":"finish","findings":[],"unresolved":[]}),
            10,
        )
    }

    fn response_body(model: &str, decision: serde_json::Value, completion_tokens: u64) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "id":"fixture-1", "object":"chat.completion", "created":1, "model":model,
            "choices":[{"index":0,"message":{"role":"assistant","content":decision.to_string()},"finish_reason":"stop"}],
            "usage":{"prompt_tokens":20,"completion_tokens":completion_tokens,"total_tokens":20 + completion_tokens}
        })).unwrap()
    }

    struct ExactSource {
        observation: ReviewSourceObservation,
        reads: usize,
    }

    impl ReviewSource for ExactSource {
        fn read(
            &mut self,
            _: &ReviewRequest,
            _: &ReviewSourceSelector,
            max_bytes: usize,
        ) -> std::result::Result<ReviewSourceObservation, SourceReadFailure> {
            if self.observation.raw.len() > max_bytes {
                return Err(SourceReadFailure::Limit);
            }
            self.reads += 1;
            Ok(self.observation.clone())
        }
    }

    /// Synthetic fixture only. This does not provide durable authorization.
    #[derive(Default)]
    struct FixtureJournal {
        reservations: Vec<CallReservation>,
        records: Vec<(CallTicket, ModelCallObservation)>,
    }

    impl ReviewJournal for FixtureJournal {
        fn reserve_call(&mut self, reservation: &CallReservation) -> Result<ReservationOutcome> {
            if self.reservations.iter().any(|existing| {
                existing.attempt_id == reservation.attempt_id
                    && existing.run_generation == reservation.run_generation
                    && existing.call_index == reservation.call_index
            }) {
                return Ok(ReservationOutcome::Existing);
            }
            self.reservations.push(reservation.clone());
            Ok(ReservationOutcome::Reserved(CallTicket {
                reservation_sha256: reservation.digest()?,
                run_generation: reservation.run_generation,
                durable_version: self.reservations.len() as u64,
            }))
        }

        fn record_call(
            &mut self,
            ticket: &CallTicket,
            observation: &ModelCallObservation,
        ) -> Result<()> {
            self.records.push((ticket.clone(), observation.clone()));
            Ok(())
        }
    }

    fn controller_fixture(
        transport: &LocalHttpTransport,
        max_calls: u32,
        max_output_tokens: u32,
        timeout_ms: u32,
    ) -> (ReviewRequest, ExactSource, String, String) {
        let text = "fixture source row\n".to_owned();
        let source = ReviewSourceBinding {
            profile: "fixture-source-v1".into(),
            permit_sha256: "7".repeat(64),
        };
        let selector = ReviewSourceSelector {
            source: source.clone(),
            path: "src/fixture.rs".into(),
            start_line: 1,
            end_line: 1,
            raw_sha256: hash_bytes(text.as_bytes()),
            rendered_sha256: hash_bytes(text.as_bytes()),
            required: true,
        };
        let row_id = selector.row_id().unwrap();
        let recipe = ReviewRecipe {
            version: 1,
            model: "fixture-v1".into(),
            objective: "Review the synthetic fixture.".into(),
            provider_scope_sha256: transport.scope_sha256(),
            installed_policy_sha256: "8".repeat(64),
            policy_epoch: 1,
            source: vec![selector],
            limits: ReviewLimits {
                max_source_bytes: 4096,
                max_request_bytes: 4096,
                max_total_request_bytes: 8192,
                max_response_bytes: 4096,
                max_calls,
                max_input_tokens: 1000,
                max_output_tokens,
                max_total_tokens: 2000,
                max_cost_micros: Some(10_000),
                timeout_ms,
            },
        };
        let mut subject = CheckSubject {
            repository: "repository-1".into(),
            change: ChangeId {
                repository: "repository-1".into(),
                id: "change-1".into(),
            },
            change_version: 1,
            context_digest: "1".repeat(64),
            revision: "2".repeat(64),
            recipe_sha256: String::new(),
            toolchain_sha256: "3".repeat(64),
            environment_sha256: "4".repeat(64),
        };
        subject.recipe_sha256 = recipe.digest().unwrap();
        (
            ReviewRequest {
                attempt_id: "fixture-attempt-1".into(),
                run_generation: 1,
                subject,
                recipe,
            },
            ExactSource {
                observation: ReviewSourceObservation {
                    source,
                    raw: text.as_bytes().to_vec(),
                    rendered: text.clone(),
                    rendering: SourceRendering::ExactUtf8,
                },
                reads: 0,
            },
            row_id,
            text,
        )
    }

    fn server(
        handler: impl FnOnce(TcpStream) + Send + 'static,
    ) -> (LocalHttpTransport, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let transport = LocalHttpTransport::new(LocalHttpConfig {
            address: listener.local_addr().unwrap(),
            model: "fixture-v1".into(),
        })
        .unwrap();
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(3);
            loop {
                match listener.accept() {
                    Ok((stream, _)) => {
                        handler(stream);
                        break;
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(Instant::now() < deadline, "fixture did not receive request");
                        thread::sleep(Duration::from_millis(2));
                    }
                    Err(error) => panic!("fixture accept failed: {error}"),
                }
            }
            // The transport must make exactly one request, even after reset/redirect.
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                assert!(
                    matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock),
                    "unexpected retry"
                );
                thread::sleep(Duration::from_millis(2));
            }
        });
        (transport, handle)
    }

    fn server_calls(
        expected_calls: usize,
        mut handler: impl FnMut(usize, TcpStream) + Send + 'static,
    ) -> (LocalHttpTransport, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let transport = LocalHttpTransport::new(LocalHttpConfig {
            address: listener.local_addr().unwrap(),
            model: "fixture-v1".into(),
        })
        .unwrap();
        let handle = thread::spawn(move || {
            for index in 0..expected_calls {
                let deadline = Instant::now() + Duration::from_secs(3);
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            handler(index, stream);
                            break;
                        }
                        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                            assert!(
                                Instant::now() < deadline,
                                "fixture did not receive call {index}"
                            );
                            thread::sleep(Duration::from_millis(2));
                        }
                        Err(error) => panic!("fixture accept failed: {error}"),
                    }
                }
            }
            let deadline = Instant::now() + Duration::from_millis(100);
            while Instant::now() < deadline {
                assert!(
                    matches!(listener.accept(),Err(error) if error.kind()==std::io::ErrorKind::WouldBlock),
                    "unexpected retry or extra controller call"
                );
                thread::sleep(Duration::from_millis(2));
            }
        });
        (transport, handle)
    }

    #[tokio::test]
    async fn local_http_collects_exact_bounded_response_without_host_credentials() {
        let body = valid_body();
        let expected = body.clone();
        let (transport, handle) = server(move |mut stream| {
            request(&mut stream);
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let observed = transport
            .invoke(&call(1000, 4096), &ReviewCancellation::default())
            .await;
        assert_eq!(observed.state, CallState::Acknowledged);
        assert_eq!(observed.basis, TransportBasis::LocalHttpUnqualified);
        assert_eq!(observed.captured_response, expected);
        assert_eq!(observed.captured_response_sha256, hash_bytes(&expected));
        assert!(observed.response_capture_complete);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn redirect_and_connection_reset_are_uncertain_without_retry() {
        for redirect in [false, true] {
            let (transport, handle) = server(move |mut stream| {
                request(&mut stream);
                if redirect {
                    stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:9/private\r\nContent-Length: 0\r\n\r\n").unwrap();
                }
            });
            let observed = transport
                .invoke(&call(1000, 4096), &ReviewCancellation::default())
                .await;
            assert_eq!(observed.state, CallState::Uncertain);
            assert_eq!(
                observed.failure,
                Some(if redirect {
                    ModelFailure::HttpStatus
                } else {
                    ModelFailure::Connection
                })
            );
            assert!(observed.reply.is_none());
            handle.join().unwrap();
        }
    }

    #[tokio::test]
    async fn chunked_overflow_keeps_only_the_bounded_prefix() {
        let (transport, handle) = server(|mut stream| {
            request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n100\r\n").unwrap();
            let _ = stream.write_all(&[b'x'; 256]);
            let _ = stream.write_all(b"\r\n0\r\n\r\n");
        });
        let observed = transport
            .invoke(&call(1000, 32), &ReviewCancellation::default())
            .await;
        assert_eq!(observed.failure, Some(ModelFailure::ResponseLimit));
        assert_eq!(observed.captured_response, vec![b'x'; 32]);
        assert!(!observed.response_capture_complete);
        assert_eq!(observed.state, CallState::Uncertain);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn total_deadline_stops_a_trickling_response_and_retains_uncertainty() {
        let (transport, handle) = server(|mut stream| {
            request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n").unwrap();
            for _ in 0..30 {
                if stream.write_all(b"1\r\nx\r\n").is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let observed = transport
            .invoke(&call(60, 4096), &ReviewCancellation::default())
            .await;
        assert_eq!(observed.failure, Some(ModelFailure::Deadline));
        assert_eq!(observed.state, CallState::Uncertain);
        assert!(!observed.response_capture_complete);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn cancellation_before_send_is_definitely_not_started() {
        let transport = LocalHttpTransport::new(LocalHttpConfig {
            address: "127.0.0.1:9".parse().unwrap(),
            model: "fixture-v1".into(),
        })
        .unwrap();
        let cancel = ReviewCancellation::default();
        cancel.cancel();
        let observed = transport.invoke(&call(1000, 4096), &cancel).await;
        assert_eq!(observed.state, CallState::NotStarted);
        assert_eq!(observed.failure, Some(ModelFailure::Cancelled));
    }

    #[tokio::test]
    async fn controller_records_invalid_http_replies_once_without_reading_source() {
        for case in 0..3 {
            let mut value: serde_json::Value = serde_json::from_slice(&valid_body()).unwrap();
            match case {
                0 => value["model"] = serde_json::json!("other-model"),
                1 => {
                    value.as_object_mut().unwrap().remove("usage");
                }
                _ => value["choices"][0]["message"]["tool_calls"] = serde_json::json!([]),
            }
            let body = serde_json::to_vec(&value).unwrap();
            let expected_body = body.clone();
            let (transport, handle) = server(move |mut stream| {
                request(&mut stream);
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            });
            let (controller_request, mut source, _, _) =
                controller_fixture(&transport, 2, 128, 1000);
            let mut journal = FixtureJournal::default();
            let record = crate::reviewer::ReviewController::run(
                controller_request,
                &mut source,
                &mut journal,
                &transport,
                &ReviewCancellation::default(),
            )
            .await
            .unwrap();
            assert_eq!(record.completion, ReviewCompletion::Incomplete);
            assert_eq!(record.stop, ReviewStop::Transport);
            assert_eq!(record.calls.len(), 1);
            assert_eq!(journal.reservations.len(), 1);
            assert_eq!(journal.records.len(), 1);
            assert_eq!(source.reads, 0);
            assert_eq!(record.coverage[0].exposure, RowExposure::Unread);
            let observed = &record.calls[0].observation;
            assert_eq!(observed.state, CallState::Uncertain);
            assert_eq!(
                observed.failure,
                Some(if case == 0 {
                    ModelFailure::Identity
                } else {
                    ModelFailure::InvalidResponse
                })
            );
            assert_eq!(observed.captured_response, expected_body);
            assert_eq!(
                observed.captured_response_sha256,
                hash_bytes(&expected_body)
            );
            assert!(observed.response_capture_complete);
            assert_eq!(observed.http_status, Some(200));
            handle.join().unwrap();
        }
    }

    #[tokio::test]
    async fn controller_maps_http_deadline_and_output_limit_to_incomplete_stops() {
        let (transport, handle) = server(|mut stream| {
            request(&mut stream);
            stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n").unwrap();
            thread::sleep(Duration::from_millis(150));
        });
        let (controller_request, mut source, _, _) = controller_fixture(&transport, 1, 128, 60);
        let mut journal = FixtureJournal::default();
        let record = crate::reviewer::ReviewController::run(
            controller_request,
            &mut source,
            &mut journal,
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(record.completion, ReviewCompletion::Incomplete);
        assert_eq!(record.stop, ReviewStop::Deadline);
        assert_eq!(
            record.calls[0].observation.failure,
            Some(ModelFailure::Deadline)
        );
        assert_eq!(journal.reservations.len(), 1);
        assert_eq!(journal.records.len(), 1);
        handle.join().unwrap();

        let body = valid_body();
        let expected_body = body.clone();
        let (transport, handle) = server(move |mut stream| {
            request(&mut stream);
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let (controller_request, mut source, _, _) = controller_fixture(&transport, 1, 9, 1000);
        let mut journal = FixtureJournal::default();
        let record = crate::reviewer::ReviewController::run(
            controller_request,
            &mut source,
            &mut journal,
            &transport,
            &ReviewCancellation::default(),
        )
        .await
        .unwrap();
        assert_eq!(record.completion, ReviewCompletion::Incomplete);
        assert_eq!(record.stop, ReviewStop::TokenLimit);
        assert_eq!(
            record.calls[0].observation.failure,
            Some(ModelFailure::OutputLimit)
        );
        assert_eq!(record.calls[0].observation.captured_response, expected_body);
        assert_eq!(journal.reservations.len(), 1);
        assert_eq!(journal.records.len(), 1);
        handle.join().unwrap();
    }

    #[tokio::test]
    async fn controller_keeps_prepared_source_possibly_disclosed_after_send_cancellation() {
        let received = Arc::new(AtomicBool::new(false));
        let server_received = received.clone();
        let expected_row = Arc::new(std::sync::Mutex::new(None::<(String, String)>));
        let server_row = expected_row.clone();
        let (transport, handle) = server_calls(2, move |index, mut stream| {
            let request_bytes = request(&mut stream);
            if index == 0 {
                let (row_id, _) = server_row.lock().unwrap().clone().unwrap();
                let body = response_body(
                    "fixture-v1",
                    serde_json::json!({"action":"request_rows","row_ids":[row_id]}),
                    3,
                );
                write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",body.len()).unwrap();
                stream.write_all(&body).unwrap();
            } else {
                let (_, text) = server_row.lock().unwrap().clone().unwrap();
                let body_offset = request_bytes
                    .windows(4)
                    .position(|window| window == b"\r\n\r\n")
                    .unwrap()
                    + 4;
                let request_json: serde_json::Value =
                    serde_json::from_slice(&request_bytes[body_offset..]).unwrap();
                let input: serde_json::Value =
                    serde_json::from_str(request_json["messages"][1]["content"].as_str().unwrap())
                        .unwrap();
                assert_eq!(
                    input["supplied_rows"][0]["text"].as_str(),
                    Some(text.as_str())
                );
                server_received.store(true, Ordering::SeqCst);
                let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nx\r\n");
                thread::sleep(Duration::from_millis(150));
            }
        });
        let (controller_request, mut source, row_id, text) =
            controller_fixture(&transport, 2, 128, 1000);
        *expected_row.lock().unwrap() = Some((row_id.clone(), text));
        let cancel = ReviewCancellation::default();
        let trigger = cancel.clone();
        let canceller = tokio::spawn(async move {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
            while !received.load(Ordering::SeqCst) {
                assert!(tokio::time::Instant::now() < deadline);
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
            trigger.cancel();
        });
        let mut journal = FixtureJournal::default();
        let record = crate::reviewer::ReviewController::run(
            controller_request,
            &mut source,
            &mut journal,
            &transport,
            &cancel,
        )
        .await
        .unwrap();
        assert_eq!(record.completion, ReviewCompletion::Cancelled);
        assert_eq!(record.stop, ReviewStop::Cancelled);
        assert_eq!(record.calls.len(), 2);
        assert_eq!(record.calls[0].observation.state, CallState::Acknowledged);
        assert_eq!(record.calls[1].observation.state, CallState::Uncertain);
        assert_eq!(
            record.calls[1].observation.failure,
            Some(ModelFailure::Cancelled)
        );
        assert!(!record.calls[1].observation.response_capture_complete);
        assert_eq!(record.coverage[0].exposure, RowExposure::PossiblyDisclosed);
        assert_eq!(source.reads, 1);
        assert_eq!(journal.reservations.len(), 2);
        assert_eq!(journal.records.len(), 2);
        assert_eq!(journal.reservations[1].source_row_ids, vec![row_id]);
        canceller.await.unwrap();
        handle.join().unwrap();
    }

    #[test]
    fn endpoint_profile_rejects_remote_or_unspecified_addresses() {
        for address in ["0.0.0.0:8080", "192.0.2.1:8080", "[::]:8080", "127.0.0.1:0"] {
            assert!(
                LocalHttpTransport::new(LocalHttpConfig {
                    address: address.parse().unwrap(),
                    model: "fixture-v1".into()
                })
                .is_err()
            );
        }
    }

    #[tokio::test]
    async fn complete_http_capture_must_decode_to_the_entire_typed_reply() {
        let body = valid_body();
        let (transport, handle) = server(move |mut stream| {
            request(&mut stream);
            write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
            stream.write_all(&body).unwrap();
        });
        let prepared = call(1000, 4096);
        let observed = transport
            .invoke(&prepared, &ReviewCancellation::default())
            .await;
        assert!(observed.consistent_with(&prepared, TransportBasis::LocalHttpUnqualified));
        for case in 0..4 {
            let mut contradictory = observed.clone();
            let reply = contradictory.reply.as_mut().unwrap();
            match case {
                0 => reply.content_sha256 = "6".repeat(64),
                1 => {
                    reply.decision = ReviewDecision::Finish {
                        findings: Vec::new(),
                        unresolved: vec!["Different decision".into()],
                    };
                    reply.content_sha256 =
                        hash_bytes(&serde_json::to_vec(&reply.decision).unwrap());
                }
                2 => {
                    reply.usage.input_tokens += 1;
                    reply.usage.total_tokens += 1;
                }
                3 => reply.system_fingerprint = Some("different-route".into()),
                _ => unreachable!(),
            }
            assert!(
                !contradictory.consistent_with(&prepared, TransportBasis::LocalHttpUnqualified),
                "case {case}"
            );
        }
        handle.join().unwrap();
    }
}
