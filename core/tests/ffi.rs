//! A whole call driven through the C face only — the functions of include/tinyrtc.h, the way a
//! platform half calls them: two contexts, a connection in each, signaling in three calls, the
//! microphone and a camera stream both ways, a data channel, the numbers.

use std::ffi::{c_char, c_void, CStr, CString};
use std::net::{IpAddr, Ipv4Addr};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tinyrtc::ffi::*;

/// What one side's callbacks saw. The `user` pointer of every callback points at one of these.
#[derive(Default)]
struct Seen {
    states: Vec<i32>,
    video_frames: u32,
    /// The NAL unit types of the last keyframe received.
    keyframe_nals: Vec<u8>,
    rotations: Vec<u16>,
    opened: Vec<(u32, String)>,
    messages: Vec<(u32, Vec<u8>, u8)>,
    targets: Vec<(u32, u32, u32)>,
    keyframes_asked: u32,
}

unsafe extern "C" fn on_event(user: *mut c_void, event: *const tinyrtc_event) {
    let seen = &*(user as *const Mutex<Seen>);
    let mut seen = seen.lock().unwrap();
    let event = &*event;
    match event.kind {
        TINYRTC_EVENT_STATE => seen.states.push(event.body.state),
        TINYRTC_EVENT_VIDEO_FRAME => {
            let video = event.body.video;
            let nals = std::slice::from_raw_parts(video.nals, video.nal_count);
            seen.video_frames += 1;
            seen.rotations.push(video.rotation);
            if video.keyframe != 0 {
                seen.keyframe_nals = nals.iter().map(|nal| nal.kind).collect();
                // A unit is its header byte and its payload: the type is in the header.
                assert!(nals.iter().all(|nal| nal.len > 1 && *nal.data & 0x1f == nal.kind));
            }
        }
        TINYRTC_EVENT_CHANNEL_OPEN => {
            let channel = event.body.channel;
            seen.opened.push((channel.id, CStr::from_ptr(channel.label).to_str().unwrap().to_owned()));
        }
        TINYRTC_EVENT_CHANNEL_MESSAGE => {
            let message = event.body.message;
            let data = std::slice::from_raw_parts(message.data, message.len).to_vec();
            seen.messages.push((message.id, data, message.binary));
        }
        _ => {}
    }
}

unsafe extern "C" fn on_target(user: *mut c_void, bitrate: u32, fps: u32, height: u32) {
    (*(user as *const Mutex<Seen>)).lock().unwrap().targets.push((bitrate, fps, height));
}

unsafe extern "C" fn on_keyframe(user: *mut c_void) {
    (*(user as *const Mutex<Seen>)).lock().unwrap().keyframes_asked += 1;
}

/// An access unit as VideoToolbox frames it: 4-byte lengths.
fn avcc(key: bool, n: u32) -> Vec<u8> {
    let mut au = Vec::new();
    let mut nal = |header: u8, len: usize| {
        au.extend_from_slice(&(len as u32 + 1).to_be_bytes());
        au.push(header);
        au.extend((0..len).map(|i| ((i as u32).wrapping_mul(31).wrapping_add(n) % 251) as u8 | 0x10));
    };
    if key {
        nal(0x67, 24);
        nal(0x68, 6);
        nal(0x65, 5_000);
    } else {
        nal(0x41, 600);
    }
    au
}

fn tone(from: usize, len: usize) -> Vec<i16> {
    (from..from + len).map(|i| ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16).collect()
}

struct Side {
    seen: Box<Mutex<Seen>>,
    context: *mut tinyrtc_context,
    connection: *mut tinyrtc_connection,
    source: *mut tinyrtc_video_source,
}

unsafe fn side() -> Side {
    let seen: Box<Mutex<Seen>> = Box::default();
    let user = &*seen as *const Mutex<Seen> as *mut c_void;
    let context = tinyrtc_context_create(std::ptr::null());
    let config = tinyrtc_connection_config {
        ice_servers: std::ptr::null(),
        ice_server_count: 0,
        relay_only: 0,
        event: Some(on_event),
        user,
    };
    let connection = connection_create_bound(context, &config, Some(IpAddr::V4(Ipv4Addr::LOCALHOST)));
    let callbacks = tinyrtc_video_source_callbacks { target: Some(on_target), keyframe: Some(on_keyframe), user };
    let source = tinyrtc_video_source_create(context, &callbacks);
    assert!(!context.is_null() && !connection.is_null() && !source.is_null());
    Side { seen, context, connection, source }
}

unsafe fn take(text: tinyrtc_text) -> CString {
    let copy = CStr::from_ptr(text.data).to_owned();
    assert_eq!(copy.as_bytes().len(), text.len);
    tinyrtc_text_free(text);
    copy
}

#[test]
fn a_call_through_the_c_face() {
    unsafe {
        assert_eq!(tinyrtc_abi_version(), 1);
        let (a, b) = (side(), side());
        let empty = || tinyrtc_text { data: std::ptr::null_mut(), len: 0 };

        // Signaling: three calls.
        let mut chat = 0u32;
        let label = CString::new("chat").unwrap();
        assert_eq!(tinyrtc_channel_open(a.connection, label.as_ptr(), std::ptr::null(), &mut chat), TINYRTC_OK);
        let mut offer = empty();
        assert_eq!(tinyrtc_connection_offer(a.connection, 0, &mut offer), TINYRTC_OK);
        let offer = take(offer);
        let mut answer = empty();
        assert_eq!(tinyrtc_connection_accept_offer(b.connection, offer.as_ptr(), &mut answer), TINYRTC_OK);
        let answer = take(answer);
        assert_eq!(tinyrtc_connection_accept_answer(a.connection, answer.as_ptr()), TINYRTC_OK);
        // The errors of the header.
        assert_eq!(tinyrtc_connection_accept_answer(a.connection, answer.as_ptr()), TINYRTC_ERR_STATE);
        let junk = CString::new("not a description").unwrap();
        assert_eq!(tinyrtc_connection_accept_offer(b.connection, junk.as_ptr(), &mut empty()), TINYRTC_ERR_SDP);
        assert_eq!(tinyrtc_connection_offer(std::ptr::null_mut(), 0, &mut empty()), TINYRTC_ERR_HANDLE);
        assert_eq!(tinyrtc_channel_send(a.connection, 777, [1u8].as_ptr(), 1, 1), TINYRTC_ERR_NOT_FOUND);

        let connected = |side: &Side| side.seen.lock().unwrap().states.last() == Some(&2);
        let start = Instant::now();
        while !(connected(&a) && connected(&b)) {
            assert!(start.elapsed() < Duration::from_secs(10), "not connected");
            std::thread::sleep(Duration::from_millis(5));
        }

        // Both send the microphone and the camera.
        for side in [&a, &b] {
            tinyrtc_connection_send_audio(side.connection, 1);
            tinyrtc_connection_send_video(side.connection, side.source);
        }
        // A new taker needs a keyframe, and the encoder is told its target at once.
        assert_eq!(a.seen.lock().unwrap().keyframes_asked, 1);
        assert_eq!(a.seen.lock().unwrap().targets.len(), 1);

        // Three seconds of a call in real time: 20 ms of sound per tick, a picture every 33 ms.
        let start = Instant::now();
        let (mut ticks, mut frames, mut heard) = (0usize, 0u32, Vec::new());
        while start.elapsed() < Duration::from_secs(3) {
            let due = start.elapsed().as_millis() as usize / 20;
            while ticks < due {
                let pcm = tone(ticks * 960, 960);
                tinyrtc_audio_push(a.context, pcm.as_ptr(), pcm.len());
                tinyrtc_audio_push(b.context, pcm.as_ptr(), pcm.len());
                let mut out = [0i16; 960];
                tinyrtc_audio_pull(b.context, out.as_mut_ptr(), out.len());
                heard.extend_from_slice(&out);
                tinyrtc_audio_pull(a.context, out.as_mut_ptr(), out.len());
                ticks += 1;
            }
            if start.elapsed().as_millis() as u32 / 33 >= frames {
                let unit = avcc(frames % 30 == 0, frames);
                let frame = tinyrtc_video_frame_in {
                    data: unit.as_ptr(),
                    len: unit.len(),
                    framing: 1,
                    capture_time_us: start.elapsed().as_micros() as i64,
                    keyframe: (frames % 30 == 0) as u8,
                    rotation: 270,
                };
                tinyrtc_video_source_send(a.source, &frame);
                tinyrtc_video_source_send(b.source, &frame);
                // A channel can send once its open event came (the header's rule), not at "connected".
                let open = !a.seen.lock().unwrap().opened.is_empty();
                if frames % 3 == 0 && open {
                    let text = format!("m{frames}");
                    assert_eq!(tinyrtc_channel_send(a.connection, chat, text.as_ptr(), text.len(), 0), TINYRTC_OK);
                }
                frames += 1;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        // The numbers of the running call: taken while the sound is still pulled (the jitter
        // buffer's level means nothing once nobody pulls).
        let mut stats = tinyrtc_stats::default();
        assert_eq!(tinyrtc_connection_stats(b.connection, &mut stats), TINYRTC_OK);
        std::thread::sleep(Duration::from_millis(300));

        // The picture: every frame, its units listed, the rotation carried.
        for side in [&a, &b] {
            let seen = side.seen.lock().unwrap();
            assert!(seen.video_frames >= frames - 4, "frames {} of {frames}", seen.video_frames);
            assert_eq!(seen.keyframe_nals, [7, 8, 5], "a keyframe is SPS, PPS, IDR");
            assert!(seen.rotations.iter().all(|r| *r == 270));
        }
        // The sound: the tone a pushed is what b pulled.
        let steady = &heard[heard.len() - 48_000..];
        let crossings = steady.windows(2).filter(|w| (w[0] < 0) != (w[1] < 0)).count();
        assert!((crossings as i32 - 880).abs() < 30, "440 Hz expected, {} crossings in a second", crossings);
        let rms = (steady.iter().map(|s| (*s as f64).powi(2)).sum::<f64>() / steady.len() as f64).sqrt();
        assert!(rms > 3000.0, "rms {rms}");
        // The channel, with the id b knows it by.
        {
            let seen = b.seen.lock().unwrap();
            assert_eq!(seen.opened.len(), 1);
            assert_eq!(seen.opened[0].1, "chat");
            assert!(seen.messages.len() as u32 >= frames / 3 - 4, "messages {}", seen.messages.len());
            assert!(seen.messages.iter().all(|(id, data, binary)| *id == seen.opened[0].0 && data.starts_with(b"m") && *binary == 0));
        }
        // The numbers.
        assert_eq!(stats.state, 2);
        assert!(stats.audio_recv_bps > 10_000 && stats.audio_send_bps > 10_000, "{stats:?}");
        assert!(stats.video_recv_bps > 50_000 && stats.video_recv_fps >= 20, "{stats:?}");
        // The level right after a pull can be anything from empty to the target and a frame: the
        // bound that matters is that the delay did not run away.
        assert!(stats.audio_jitter_buffer_ms <= 200, "{stats:?}");

        // The ceiling moves the target.
        tinyrtc_video_source_set_limits(a.source, 100_000, 15, 240);
        let last = *a.seen.lock().unwrap().targets.last().unwrap();
        assert!(last.0 <= 100_000 && last.1 <= 15 && last.2 <= 240, "{last:?}");

        // Destroying: no callback after it returns, in either order with the context.
        tinyrtc_connection_destroy(a.connection);
        assert_eq!(a.seen.lock().unwrap().states.last(), Some(&5));
        tinyrtc_video_source_destroy(a.source);
        tinyrtc_context_destroy(a.context);
        tinyrtc_context_destroy(b.context);
        tinyrtc_connection_destroy(b.connection);
        tinyrtc_video_source_destroy(b.source);
        let _: *const c_char = std::ptr::null();
    }
}
