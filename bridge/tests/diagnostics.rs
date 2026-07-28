use std::io::{self, Cursor, Read};

use serde_json::{Value, json};
use tool4d_lsp_stdio::relay::{CompatibilityState, relay_editor_stream, relay_tool4d_stream};

fn frame(body: &[u8]) -> Vec<u8> {
    let mut result = format!("Content-Length: {}\r\n\r\n", body.len()).into_bytes();

    result.extend_from_slice(body);
    result
}

fn tool4d_frame(body: &[u8]) -> Vec<u8> {
    let mut result = format!("Content-Length:{}\r\n\r\n", body.len()).into_bytes();

    result.extend_from_slice(body);
    result
}

fn json_frame(value: &Value) -> Vec<u8> {
    frame(&serde_json::to_vec(value).expect("test JSON must serialize"))
}

fn tool4d_json_frame(value: &Value) -> Vec<u8> {
    tool4d_frame(&serde_json::to_vec(value).expect("test JSON must serialize"))
}

fn relay_editor(state: &CompatibilityState, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();

    relay_editor_stream(Cursor::new(input), &mut output, state)?;

    Ok(output)
}

fn relay_tool4d(state: &CompatibilityState, input: &[u8]) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();

    relay_tool4d_stream(Cursor::new(input), &mut output, state)?;

    Ok(output)
}

fn read_single_frame(input: &[u8]) -> (usize, Vec<u8>) {
    let delimiter = b"\r\n\r\n";

    let header_end = input
        .windows(delimiter.len())
        .position(|window| window == delimiter)
        .expect("output must contain an LSP header delimiter");

    let header = &input[..header_end];

    let header_text = std::str::from_utf8(header).expect("LSP headers must be ASCII");

    let content_length = header_text
        .split("\r\n")
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;

            if name.eq_ignore_ascii_case("Content-Length") {
                Some(
                    value
                        .trim()
                        .parse::<usize>()
                        .expect("Content-Length must be an integer"),
                )
            } else {
                None
            }
        })
        .expect("frame must contain Content-Length");

    let body_start = header_end + delimiter.len();
    let body_end = body_start + content_length;

    assert!(
        input.len() >= body_end,
        "output body is shorter than Content-Length"
    );

    assert_eq!(input.len(), body_end, "test expected exactly one LSP frame");

    (content_length, input[body_start..body_end].to_vec())
}

fn read_single_json_frame(input: &[u8]) -> Value {
    let (content_length, body) = read_single_frame(input);

    assert_eq!(
        content_length,
        body.len(),
        "Content-Length must use body byte length"
    );

    serde_json::from_slice(&body).expect("frame body must contain JSON")
}

fn diagnostic_request(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "textDocument/diagnostic",
        "params": {
            "textDocument": {
                "uri": "file:///test.4dm"
            }
        }
    })
}

fn null_response(id: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": null
    })
}

struct ChunkedReader<R> {
    inner: R,
    chunk_size: usize,
}

impl<R> ChunkedReader<R> {
    fn new(inner: R, chunk_size: usize) -> Self {
        Self { inner, chunk_size }
    }
}

impl<R: Read> Read for ChunkedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let limit = buffer.len().min(self.chunk_size);
        self.inner.read(&mut buffer[..limit])
    }
}

#[test]
fn repairs_null_diagnostic_response_with_numeric_id() {
    let state = CompatibilityState::default();

    let request = json_frame(&diagnostic_request(json!(10)));

    relay_editor(&state, &request).expect("diagnostic request should be relayed");

    let response = tool4d_json_frame(&null_response(json!(10)));

    let output = relay_tool4d(&state, &response).expect("diagnostic response should be relayed");

    assert_eq!(
        read_single_json_frame(&output),
        json!({
            "jsonrpc": "2.0",
            "id": 10,
            "result": {
                "kind": "full",
                "items": []
            }
        })
    );
}

#[test]
fn repairs_null_diagnostic_response_with_string_id() {
    let state = CompatibilityState::default();

    let request = json_frame(&diagnostic_request(json!("diagnostic-11")));

    relay_editor(&state, &request).expect("diagnostic request should be relayed");

    let response = tool4d_json_frame(&null_response(json!("diagnostic-11")));

    let output = relay_tool4d(&state, &response).expect("diagnostic response should be relayed");

    assert_eq!(
        read_single_json_frame(&output),
        json!({
            "jsonrpc": "2.0",
            "id": "diagnostic-11",
            "result": {
                "kind": "full",
                "items": []
            }
        })
    );
}

#[test]
fn unrelated_null_response_is_unchanged() {
    let state = CompatibilityState::default();

    let request = json!({
        "jsonrpc": "2.0",
        "id": 12,
        "method": "shutdown",
        "params": null
    });

    relay_editor(&state, &json_frame(&request)).expect("shutdown request should be relayed");

    let response_body = br#"{
  "jsonrpc": "2.0",
  "id": 12,
  "result": null
}"#;

    let output = relay_tool4d(&state, &tool4d_frame(response_body))
        .expect("shutdown response should be relayed");

    let (_, output_body) = read_single_frame(&output);

    assert_eq!(output_body, response_body);
}

#[test]
fn valid_full_diagnostic_report_is_unchanged() {
    let state = CompatibilityState::default();

    relay_editor(&state, &json_frame(&diagnostic_request(json!(13))))
        .expect("diagnostic request should be relayed");

    let response_body = br#"{"jsonrpc":"2.0","id":13,"result":{"kind":"full","items":[{"range":{"start":{"line":0,"character":0},"end":{"line":0,"character":1}},"message":"Example"}]}}"#;

    let output = relay_tool4d(&state, &tool4d_frame(response_body))
        .expect("valid diagnostic report should be relayed");

    let (_, output_body) = read_single_frame(&output);

    assert_eq!(output_body, response_body);
}

#[test]
fn valid_unchanged_diagnostic_report_is_unchanged() {
    let state = CompatibilityState::default();

    relay_editor(&state, &json_frame(&diagnostic_request(json!(14))))
        .expect("diagnostic request should be relayed");

    let response_body =
        br#"{"jsonrpc":"2.0","id":14,"result":{"kind":"unchanged","resultId":"previous-result"}}"#;

    let output = relay_tool4d(&state, &tool4d_frame(response_body))
        .expect("unchanged diagnostic report should be relayed");

    let (_, output_body) = read_single_frame(&output);

    assert_eq!(output_body, response_body);
}

#[test]
fn diagnostic_error_response_is_unchanged() {
    let state = CompatibilityState::default();

    relay_editor(&state, &json_frame(&diagnostic_request(json!(15))))
        .expect("diagnostic request should be relayed");

    let error_response_body =
        br#"{"jsonrpc":"2.0","id":15,"error":{"code":-32603,"message":"Internal error"}}"#;

    let first_output = relay_tool4d(&state, &tool4d_frame(error_response_body))
        .expect("error response should be relayed");

    let (_, first_body) = read_single_frame(&first_output);

    assert_eq!(first_body, error_response_body);

    /*
     * The error response consumes the pending request ID. A subsequent null
     * response with the same ID must not be rewritten.
     */
    let repeated_body = br#"{"jsonrpc":"2.0","id":15,"result":null}"#;

    let second_output = relay_tool4d(&state, &tool4d_frame(repeated_body))
        .expect("repeated response should be relayed");

    let (_, second_body) = read_single_frame(&second_output);

    assert_eq!(second_body, repeated_body);
}

#[test]
fn handles_out_of_order_diagnostic_responses() {
    let state = CompatibilityState::default();

    let mut requests = json_frame(&diagnostic_request(json!(20)));

    requests.extend_from_slice(&json_frame(&diagnostic_request(json!(21))));

    relay_editor(&state, &requests).expect("diagnostic requests should be relayed");

    let response_21 = tool4d_json_frame(&null_response(json!(21)));

    let output_21 = relay_tool4d(&state, &response_21).expect("response 21 should be relayed");

    assert_eq!(
        read_single_json_frame(&output_21)["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );

    let response_20 = tool4d_json_frame(&null_response(json!(20)));

    let output_20 = relay_tool4d(&state, &response_20).expect("response 20 should be relayed");

    assert_eq!(
        read_single_json_frame(&output_20)["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );
}

#[test]
fn repeated_response_is_not_rewritten() {
    let state = CompatibilityState::default();

    relay_editor(&state, &json_frame(&diagnostic_request(json!(22))))
        .expect("diagnostic request should be relayed");

    let first_response = tool4d_json_frame(&null_response(json!(22)));

    let first_output =
        relay_tool4d(&state, &first_response).expect("first response should be relayed");

    assert_eq!(
        read_single_json_frame(&first_output)["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );

    let repeated_body = br#"{"jsonrpc":"2.0","id":22,"result":null}"#;

    let second_output = relay_tool4d(&state, &tool4d_frame(repeated_body))
        .expect("repeated response should be relayed");

    let (_, second_body) = read_single_frame(&second_output);

    assert_eq!(second_body, repeated_body);
}

#[test]
fn diagnostic_notification_is_not_tracked() {
    let state = CompatibilityState::default();

    let notification = json!({
        "jsonrpc": "2.0",
        "method": "textDocument/diagnostic",
        "params": {
            "textDocument": {
                "uri": "file:///test.4dm"
            }
        }
    });

    relay_editor(&state, &json_frame(&notification)).expect("notification should be relayed");

    let response_body = br#"{"jsonrpc":"2.0","id":23,"result":null}"#;

    let output =
        relay_tool4d(&state, &tool4d_frame(response_body)).expect("response should be relayed");

    let (_, output_body) = read_single_frame(&output);

    assert_eq!(output_body, response_body);
}

#[test]
fn malformed_json_payload_is_forwarded_unchanged() {
    let state = CompatibilityState::default();

    let malformed_body = b"{not valid JSON";

    let editor_output = relay_editor(&state, &frame(malformed_body))
        .expect("malformed editor payload should be relayed");

    let (_, editor_body) = read_single_frame(&editor_output);

    assert_eq!(editor_body, malformed_body);

    let tool4d_output = relay_tool4d(&state, &tool4d_frame(malformed_body))
        .expect("malformed tool4d payload should be relayed");

    let (_, tool4d_body) = read_single_frame(&tool4d_output);

    assert_eq!(tool4d_body, malformed_body);
}

#[test]
fn rewritten_content_length_uses_utf8_byte_length() {
    let state = CompatibilityState::default();

    let request = json!({
        "jsonrpc": "2.0",
        "id": "診断-24",
        "method": "textDocument/diagnostic",
        "params": {
            "textDocument": {
                "uri": "file:///プロジェクト/test.4dm"
            }
        }
    });

    relay_editor(&state, &json_frame(&request)).expect("Unicode request should be relayed");

    let response = json!({
        "jsonrpc": "2.0",
        "id": "診断-24",
        "result": null
    });

    let output = relay_tool4d(&state, &tool4d_json_frame(&response))
        .expect("Unicode response should be relayed");

    let (content_length, body) = read_single_frame(&output);

    assert_eq!(content_length, body.len());

    let value: Value = serde_json::from_slice(&body).expect("rewritten response must be JSON");

    assert_eq!(
        value["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );
}

#[test]
fn handles_fragmented_request_and_response_reads() {
    let state = CompatibilityState::default();

    let request = json_frame(&diagnostic_request(json!(25)));

    let mut editor_output = Vec::new();

    relay_editor_stream(
        ChunkedReader::new(Cursor::new(request), 1),
        &mut editor_output,
        &state,
    )
    .expect("fragmented request should be relayed");

    let response = tool4d_json_frame(&null_response(json!(25)));

    let mut tool4d_output = Vec::new();

    relay_tool4d_stream(
        ChunkedReader::new(Cursor::new(response), 1),
        &mut tool4d_output,
        &state,
    )
    .expect("fragmented response should be relayed");

    assert_eq!(
        read_single_json_frame(&tool4d_output)["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );
}

#[test]
fn transformed_response_has_canonical_header() {
    let state = CompatibilityState::default();

    relay_editor(&state, &json_frame(&diagnostic_request(json!(26))))
        .expect("diagnostic request should be relayed");

    let response = tool4d_json_frame(&null_response(json!(26)));

    let output = relay_tool4d(&state, &response).expect("diagnostic response should be relayed");

    assert!(
        output.starts_with(b"Content-Length: "),
        "output should use a canonical Content-Length header"
    );

    let (content_length, body) = read_single_frame(&output);

    assert_eq!(content_length, body.len());

    assert_eq!(
        serde_json::from_slice::<Value>(&body).expect("response should contain JSON")["result"],
        json!({
            "kind": "full",
            "items": []
        })
    );
}
