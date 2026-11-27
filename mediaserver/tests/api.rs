//! The private HTTP API on its own: the secret, the routes, the errors. No media.

mod common;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use common::*;
use tinyrtc_mediaserver::Server;

#[test]
fn every_request_needs_the_secret() {
    let (server, _log) = server(|_| {});
    let addr = server.http_addr();
    for secret in [None, Some("wrong"), Some("test-secre"), Some("test-secret2"), Some("")] {
        let (status, body) = http_raw(addr, "POST", "/rooms", secret, "");
        assert_eq!(status, 401, "{secret:?}: {body}");
        assert!(body.starts_with("{\"error\":"), "{body}");
    }
    // Not even the routes are told apart without it: the same answer, header for header.
    assert_eq!(http_raw(addr, "GET", "/nothing", None, "").0, 401);
    let undated = |r: String| r.lines().filter(|l| !l.to_ascii_lowercase().starts_with("date:")).collect::<Vec<_>>().join("\n");
    let unknown = undated(raw(&server, b"GET /nothing HTTP/1.1\r\n\r\n"));
    for known in ["GET /rooms", "POST /rooms", "PUT /calls/0000000000000000", "GET /rooms/x/publish/y"] {
        assert_eq!(undated(raw(&server, format!("{known} HTTP/1.1\r\n\r\n").as_bytes())), unknown, "{known}");
    }
    for header in ["Authorization: Basic dGVzdC1zZWNyZXQ=", "Authorization: bearer test-secret", "Authorization: Bearer"] {
        let response = raw(&server, format!("POST /rooms HTTP/1.1\r\n{header}\r\n\r\n").as_bytes());
        // Header names are case-insensitive (RFC 9110 §5.1).
        assert!(response.starts_with("HTTP/1.1 401") && response.to_ascii_lowercase().contains("\r\nwww-authenticate: bearer\r\n"), "{header}: {response}");
    }
    let response = raw(&server, format!("POST /rooms HTTP/1.1\r\nAuthorization: Bearer  {SECRET} \r\n\r\n").as_bytes());
    assert!(response.starts_with("HTTP/1.1 201"), "spaces around the token: {response}");
    assert_eq!(http(&server, "POST", "/rooms", "").0, 201);
}

/// A request as bytes, the whole response as text. Without `Connection: close` in the request:
/// the server closes the connection after its one response.
fn raw(server: &Server, request: &[u8]) -> String {
    let mut stream = TcpStream::connect(server.http_addr()).unwrap();
    stream.set_read_timeout(Some(Duration::from_secs(15))).unwrap();
    stream.write_all(request).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).expect("the response, then the end of the stream");
    response
}

/// A response's header value, by a case-insensitive name.
fn header<'a>(response: &'a str, name: &str) -> Option<&'a str> {
    let head = response.split("\r\n\r\n").next()?;
    head.lines().skip(1).find_map(|l| l.split_once(':').filter(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.trim()))
}

#[test]
fn every_error_is_json_the_router_s_own_too() {
    let (server, _log) = server(|_| {});
    let room = create_room(&server);
    for (method, path, status) in [
        ("GET", "/nothing".to_owned(), 404),
        ("POST", "/rooms/".to_owned(), 404),
        ("GET", format!("/rooms/{room}/subscribe/a"), 404),
        ("GET", "/calls".to_owned(), 405),
        ("DELETE", "/rooms".to_owned(), 405),
        ("PATCH", format!("/rooms/{room}"), 405),
        ("GET", format!("/rooms/{room}/publish/alice"), 405),
        ("PUT", format!("/rooms/{room}/participants/alice"), 405),
        // A segment that does not percent-decode to UTF-8.
        ("GET", "/rooms/%FF".to_owned(), 400),
    ] {
        let response = raw(&server, format!("{method} {path} HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\n\r\n").as_bytes());
        assert!(response.starts_with(&format!("HTTP/1.1 {status} ")), "{method} {path}: {response}");
        assert_eq!(header(&response, "content-type"), Some("application/json"), "{method} {path}: {response}");
        let body = response.split_once("\r\n\r\n").unwrap().1;
        assert!(body.starts_with("{\"error\":\"") && body.ends_with("\"}"), "{method} {path}: {body}");
    }

    // Malformed JSON is told from JSON of the wrong shape.
    let (status, body) = http(&server, "POST", "/calls", "{\"ufrags\": [\"abcd\", ");
    assert!(status == 400 && body.starts_with("{\"error\":\"malformed JSON: "), "{body}");
    for wrong in ["{\"ufrags\": \"abcd efgh\"}", "{\"ufrags\": [\"abcd\", \"efgh\"], \"ufrags\": [\"ijkl\", \"mnop\"]}", "null"] {
        let (status, body) = http(&server, "POST", "/calls", wrong);
        assert_eq!((status, body.as_str()), (400, "{\"error\":\"expected {\\\"ufrags\\\": [\\\"<a>\\\", \\\"<b>\\\"]}\"}"), "{wrong}");
    }
    // Fields it does not know are ignored.
    let (status, body) = http(&server, "POST", "/calls", "{\"ufrags\": [\"abcd\", \"efgh\"], \"note\": {\"x\": [1, 2.5e3, null]}}");
    assert_eq!(status, 201, "{body}");
}

#[test]
fn the_bodies_have_their_types_and_a_204_has_none() {
    let (server, _log) = server(|_| {});
    let auth = format!("Authorization: Bearer {SECRET}");
    let response = raw(&server, format!("POST /rooms HTTP/1.1\r\n{auth}\r\n\r\n").as_bytes());
    assert!(response.starts_with("HTTP/1.1 201 Created\r\n"), "{response}");
    assert_eq!(header(&response, "content-type"), Some("application/json"));
    assert_eq!(header(&response, "connection"), Some("close"));
    let room = field(&response, "id").unwrap().to_owned();

    let offer = Phone::new("x", false).conn.offer(false).unwrap();
    let request = format!("POST /rooms/{room}/publish/alice HTTP/1.1\r\n{auth}\r\nContent-Length: {}\r\n\r\n{offer}", offer.len());
    let response = raw(&server, request.as_bytes());
    assert!(response.starts_with("HTTP/1.1 200 OK\r\n"), "{response}");
    assert_eq!(header(&response, "content-type"), Some("application/sdp"));
    assert!(response.split_once("\r\n\r\n").unwrap().1.starts_with("v=0\r\n"), "{response}");

    let response = raw(&server, format!("DELETE /rooms/{room} HTTP/1.1\r\n{auth}\r\n\r\n").as_bytes());
    assert!(response.starts_with("HTTP/1.1 204 No Content\r\n") && response.ends_with("\r\n\r\n"), "{response}");
    assert_eq!(header(&response, "content-type"), None);
    assert_eq!(header(&response, "content-length"), None, "RFC 9110 §8.6");
}

#[test]
fn a_body_over_the_limit_is_refused_before_it_is_sent() {
    let (server, _log) = server(|_| {});
    // Only the head: the answer comes without the body (and without a 100 Continue), with the
    // secret or without (the size is looked at first, as before the secret is).
    for auth in [format!("Authorization: Bearer {SECRET}\r\n"), String::new()] {
        for expect in ["", "Expect: 100-continue\r\n"] {
            let head = format!("POST /calls HTTP/1.1\r\n{auth}{expect}Content-Length: {}\r\n\r\n", 256 * 1024 + 1);
            let response = raw(&server, head.as_bytes());
            assert!(response.starts_with("HTTP/1.1 413 "), "{auth:?} {expect:?}: {response}");
            assert!(response.ends_with("{\"error\":\"the body is too large\"}"), "{response}");
        }
    }
    // At the limit it is read whole.
    let body = format!("{{\"ufrags\":[\"abcd\",\"efgh\"]}}{}", " ".repeat(256 * 1024 - 26));
    assert_eq!(body.len(), 256 * 1024);
    assert_eq!(http(&server, "POST", "/calls", &body).0, 201);
}

#[test]
fn stopping_frees_both_ports() {
    // Stopped by the future given to stop_on (main.rs gives it SIGTERM / Ctrl-C): wait returns.
    let (stopped, log) = server(|_| {});
    let (udp, http) = (stopped.udp_addr(), stopped.http_addr());
    let start = std::time::Instant::now();
    stopped.stop_on(async { tokio::time::sleep(Duration::from_millis(300)).await });
    assert!(stopped.wait(), "a stop, not a death");
    assert!(start.elapsed() >= Duration::from_millis(300));
    assert!(log.try_iter().any(|l| l == "stopped"));
    std::net::UdpSocket::bind(udp).expect("the UDP port is free");
    std::net::TcpListener::bind(http).expect("the API's port is free");

    // Dropped.
    let (dropped, _log) = server(|_| {});
    let (udp, http) = (dropped.udp_addr(), dropped.http_addr());
    drop(dropped);
    std::net::UdpSocket::bind(udp).expect("the UDP port is free");
    std::net::TcpListener::bind(http).expect("the API's port is free");
}

#[test]
fn a_chunked_body_is_read() {
    let (server, _log) = server(|_| {});
    let json = "{\"ufrags\":[\"abcd\",\"efgh\"]}";
    let (first, second) = json.split_at(10);
    let request = format!(
        "POST /calls HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{first}\r\n{:x}\r\n{second}\r\n0\r\n\r\n",
        first.len(),
        second.len()
    );
    let response = raw(&server, request.as_bytes());
    assert!(response.starts_with("HTTP/1.1 201 "), "{response}");
}

#[test]
fn unknown_things_are_404_and_bad_input_is_400() {
    let (server, _log) = server(|_| {});
    let room = create_room(&server);
    let offer = Phone::new("x", false).conn.offer(false).unwrap();

    for (method, path) in [
        ("GET", "/nothing".to_owned()),
        ("GET", "/".to_owned()),
        ("GET", "/calls/0000000000000000".to_owned()),
        ("GET", "/calls/not-an-id".to_owned()),
        ("DELETE", "/calls/0000000000000001".to_owned()),
        ("GET", "/rooms/0000000000000000".to_owned()),
        ("DELETE", "/rooms/ffffffffffffffff".to_owned()),
        ("DELETE", format!("/rooms/{room}/participants/nobody")),
        ("GET", format!("/rooms/{room}/extra")),
    ] {
        let (status, body) = http(&server, method, &path, "");
        assert_eq!(status, 404, "{method} {path}: {body}");
        assert!(body.starts_with("{\"error\":\""), "{body}");
    }
    let (status, body) = http(&server, "POST", "/rooms/0000000000000000/publish/alice", &offer);
    assert_eq!(status, 404, "publish into no room: {body}");
    let (status, body) = http(&server, "POST", &format!("/rooms/{room}/subscribe/bob/alice"), &offer);
    assert_eq!((status, body.as_str()), (404, "{\"error\":\"alice is not publishing\"}"));
    assert_eq!(http(&server, "GET", "/rooms", "").0, 405);
    assert_eq!(http(&server, "PUT", &format!("/rooms/{room}"), "").0, 405);

    // Bad SDP: not SDP at all; SDP that str0m cannot answer (no fingerprint, no m-lines).
    let no_fingerprint: String = offer.lines().filter(|l| !l.starts_with("a=fingerprint")).map(|l| format!("{l}\r\n")).collect();
    let no_media: String = offer.split("m=audio").next().unwrap().to_owned();
    for bad in ["hello", "", "v=0\r\n", no_fingerprint.as_str(), no_media.as_str()] {
        let (status, body) = http(&server, "POST", &format!("/rooms/{room}/publish/alice"), bad);
        assert_eq!(status, 400, "{bad:?}: {body}");
    }
    let (status, body) = http(&server, "GET", &format!("/rooms/{room}"), "");
    assert_eq!((status, body.as_str()), (200, "{\"participants\":[]}"), "a refused offer leaves nothing behind");
    for bad_user in ["a%20b", "a.b/c", &"x".repeat(65)] {
        let (status, _) = http(&server, "POST", &format!("/rooms/{room}/publish/{bad_user}"), &offer);
        assert!(status == 400 || status == 404, "{bad_user}: {status}");
    }
    let (status, _) = http(&server, "POST", &format!("/rooms/{room}/publish/alice"), &offer);
    assert_eq!(status, 200);
    let other = Phone::new("y", false).conn.offer(false).unwrap();
    let (status, body) = http(&server, "POST", &format!("/rooms/{room}/subscribe/alice/alice"), &other);
    assert_eq!(status, 400, "{body}");

    // Calls: malformed JSON, the wrong shape, bad ufrags.
    for bad in [
        "",
        "{",
        "{\"ufrags\": [\"abcd\", \"efgh\"],}",
        "{'ufrags': ['abcd', 'efgh']}",
        "[\"abcd\", \"efgh\"]",
        "{\"ufrags\": [\"abcd\"]}",
        "{\"ufrags\": [\"abcd\", \"efgh\", \"ijkl\"]}",
        "{\"ufrags\": [\"abcd\", 5]}",
        "{\"ufrags\": \"abcd efgh\"}",
        "{\"ufrags\": [\"abc\", \"efgh\"]}",
        "{\"ufrags\": [\"ab:cd\", \"efgh\"]}",
        "{\"ufrags\": [\"abcd\", \"abcd\"]}",
    ] {
        let (status, body) = http(&server, "POST", "/calls", bad);
        assert_eq!(status, 400, "{bad:?}: {body}");
    }
}

#[test]
fn a_reserved_pair_cannot_be_reserved_twice_and_deleting_frees_it() {
    let (server, _log) = server(|_| {});
    let (status, body) = http(&server, "POST", "/calls", "{\"ufrags\":[\"abcd\",\"efgh\"]}");
    assert_eq!(status, 201, "{body}");
    let id = field(&body, "id").unwrap().to_owned();
    for again in ["{\"ufrags\":[\"abcd\",\"efgh\"]}", "{\"ufrags\":[\"efgh\",\"abcd\"]}"] {
        let (status, body) = http(&server, "POST", "/calls", again);
        assert_eq!(status, 409, "{body}");
    }
    let (status, body) = http(&server, "GET", &format!("/calls/{id}"), "");
    assert_eq!(status, 200);
    assert!(body.starts_with("{\"sides\":[{\"ufrag\":\"abcd\",\"connected\":false,\"packets\":0,\"bytes\":0},{\"ufrag\":\"efgh\",\"connected\":false,\"packets\":0,\"bytes\":0}],\"idle_ms\":"), "{body}");
    assert_eq!(http(&server, "DELETE", &format!("/calls/{id}"), "").0, 204);
    assert_eq!(http(&server, "GET", &format!("/calls/{id}"), "").0, 404);
    assert_eq!(http(&server, "POST", "/calls", "{\"ufrags\":[\"efgh\",\"abcd\"]}").0, 201);
}

#[test]
fn rooms_are_deleted_on_request_and_when_empty_too_long() {
    let (server, log) = server(|c| c.room_empty = Duration::from_millis(400));
    let room = create_room(&server);
    assert_eq!(http(&server, "GET", &format!("/rooms/{room}"), "").1, "{\"participants\":[]}");
    until("the empty room is deleted", Duration::from_secs(3), || http(&server, "GET", &format!("/rooms/{room}"), "").0 == 404);
    let mut seen = Vec::new();
    lines(&log, &mut seen);
    assert!(seen.iter().any(|l| l.contains("deleted (empty for 0.4 s)")), "{seen:#?}");

    let room = create_room(&server);
    assert_eq!(http(&server, "DELETE", &format!("/rooms/{room}"), "").0, 204);
    assert_eq!(http(&server, "GET", &format!("/rooms/{room}"), "").0, 404);
}

#[test]
fn a_body_with_expect_continue_is_read() {
    // curl sends `Expect: 100-continue` for bodies over 1 KiB (an SDP offer is ~3 KiB).
    let (server, _log) = server(|_| {});
    let body = format!("{{\"ufrags\":[\"{}\",\"efgh\"]{}}}", "a".repeat(200), " ".repeat(2000));
    let mut stream = TcpStream::connect(server.http_addr()).unwrap();
    let head = format!(
        "POST /calls HTTP/1.1\r\nAuthorization: Bearer {SECRET}\r\nContent-Length: {}\r\nExpect: 100-continue\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).unwrap();
    let mut buf = [0u8; 25];
    stream.read_exact(&mut buf).unwrap();
    assert_eq!(&buf, b"HTTP/1.1 100 Continue\r\n\r\n");
    stream.write_all(body.as_bytes()).unwrap();
    let mut response = String::new();
    stream.read_to_string(&mut response).unwrap();
    assert!(response.starts_with("HTTP/1.1 201 Created"), "{response}");
}
