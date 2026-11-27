// Two Chrome pages in a room of the server: "alice" publishes (fake camera + microphone, H.264
// Constrained Baseline only, Opus), "bob" subscribes to her. This script is the BACKEND: it takes
// each page's offer, POSTs it to the server's private API, and gives the answer back. Driven by
// tests/browser.rs (cargo test --release --test browser -- --ignored), which starts the server.
//
//   node rooms.mjs --http 127.0.0.1:PORT --secret S --room ID [--seconds 8] [--headed]
//
// Playwright: `npm install` in this folder, or TINYRTC_PLAYWRIGHT=<a node_modules dir that has
// playwright> (tools/peer's, say). Run with node: Playwright hangs under Bun. Exit 0 = every
// check held; the last line of stdout is a JSON summary.

import { pathToFileURL } from 'node:url';
import { join } from 'node:path';

const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf('--' + name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const http = opt('http');
const secret = opt('secret');
const room = opt('room');
const seconds = Number(opt('seconds', '8'));
const headed = argv.includes('--headed');
if (!http || !secret || !room) {
  console.error('usage: node rooms.mjs --http HOST:PORT --secret S --room ID');
  process.exit(2);
}

async function loadPlaywright() {
  try {
    return await import('playwright');
  } catch {
    const dir = process.env.TINYRTC_PLAYWRIGHT;
    if (!dir) throw new Error('playwright not found: npm install here, or set TINYRTC_PLAYWRIGHT');
    return await import(pathToFileURL(join(dir, 'playwright', 'index.mjs')).href);
  }
}

const api = async (method, path, body) => {
  const res = await fetch(`http://${http}${path}`, {
    method,
    headers: { Authorization: `Bearer ${secret}`, 'Content-Type': 'application/sdp' },
    body,
  });
  return { status: res.status, text: await res.text() };
};

// The page: a few functions the script calls. H.264 is the only video codec offered:
// Constrained Baseline, packetization-mode=1 (the profile), with its RTX.
const PAGE = `<!doctype html><meta charset="utf-8"><title>tinyrtc room</title>
<video id="v" autoplay muted playsinline></video>
<script>
function profileOnly(transceiver) {
  const caps = RTCRtpReceiver.getCapabilities('video').codecs;
  const cb = caps.filter(c => c.mimeType.toLowerCase() === 'video/h264'
    && /packetization-mode=1/.test(c.sdpFmtpLine || '') && /profile-level-id=42e01f/i.test(c.sdpFmtpLine || ''));
  if (!cb.length) throw new Error('no H.264 42e01f packetization-mode=1 in this browser');
  const rtx = caps.filter(c => c.mimeType.toLowerCase() === 'video/rtx');
  transceiver.setCodecPreferences([...cb, ...rtx]);
}
async function gathered(pc) {
  if (pc.iceGatheringState === 'complete') return;
  await new Promise(r => { pc.onicegatheringstatechange = () => pc.iceGatheringState === 'complete' && r(); setTimeout(r, 3000); });
}
window.publish = async () => {
  const media = await navigator.mediaDevices.getUserMedia({ video: { width: 640, height: 480, frameRate: 30 }, audio: true });
  document.getElementById('v').srcObject = media;
  const pc = window.pc = new RTCPeerConnection();
  profileOnly(pc.addTransceiver(media.getVideoTracks()[0], { direction: 'sendonly', streams: [media] }));
  pc.addTransceiver(media.getAudioTracks()[0], { direction: 'sendonly', streams: [media] });
  await pc.setLocalDescription();
  await gathered(pc);
  return pc.localDescription.sdp;
};
window.subscribe = async () => {
  const pc = window.pc = new RTCPeerConnection();
  profileOnly(pc.addTransceiver('video', { direction: 'recvonly' }));
  pc.addTransceiver('audio', { direction: 'recvonly' });
  const remote = new MediaStream();
  pc.ontrack = (e) => { remote.addTrack(e.track); document.getElementById('v').srcObject = remote; };
  await pc.setLocalDescription();
  await gathered(pc);
  return pc.localDescription.sdp;
};
window.answer = (sdp) => window.pc.setRemoteDescription({ type: 'answer', sdp });
window.numbers = async () => {
  const report = await window.pc.getStats();
  const codec = (id) => { const c = id && report.get(id); return c ? c.mimeType : null; };
  const out = { state: window.pc.connectionState };
  report.forEach((s) => {
    if (s.type === 'outbound-rtp' && s.kind === 'video') {
      Object.assign(out, { outVideo: codec(s.codecId), framesEncoded: s.framesEncoded || 0, pliReceived: s.pliCount || 0 });
    } else if (s.type === 'outbound-rtp' && s.kind === 'audio') {
      Object.assign(out, { outAudio: codec(s.codecId), audioSent: s.packetsSent || 0 });
    } else if (s.type === 'inbound-rtp' && s.kind === 'video') {
      Object.assign(out, { inVideo: codec(s.codecId), framesDecoded: s.framesDecoded || 0, keyFramesDecoded: s.keyFramesDecoded || 0,
        frameWidth: s.frameWidth || 0, pliSent: s.pliCount || 0, nackSent: s.nackCount || 0, videoLost: s.packetsLost || 0 });
    } else if (s.type === 'inbound-rtp' && s.kind === 'audio') {
      Object.assign(out, { inAudio: codec(s.codecId), audioReceived: s.packetsReceived || 0 });
    }
  });
  return out;
};
</script>`;

// mDNS host candidates (name.local) say nothing to an ICE-lite server, which learns the
// browser's address from its checks. The server accepts an offer that has them (seen); this
// backend drops them anyway, as noise.
const withoutMdns = (sdp) => sdp.split('\r\n').filter((l) => !(l.startsWith('a=candidate') && /\.local /i.test(l))).join('\r\n');

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let browser;
let ok = false;
const summary = {};
try {
  const { chromium } = await loadPlaywright();
  browser = await chromium.launch({
    channel: 'chrome',
    headless: !headed,
    args: ['--use-fake-device-for-media-stream', '--use-fake-ui-for-media-stream', '--allow-loopback-in-peer-connection'],
  });
  const context = await browser.newContext();
  // localhost is a secure context (getUserMedia); the page never touches the network.
  const origin = 'http://localhost:9';
  await context.grantPermissions(['camera', 'microphone'], { origin });
  await context.route(`${origin}/**`, (route) => route.fulfill({ status: 200, contentType: 'text/html', body: PAGE }));
  const open = async (name) => {
    const page = await context.newPage();
    page.on('console', (m) => { if (m.type() === 'error' || m.type() === 'warning') console.log(`[${name} ${m.type()}]`, m.text()); });
    page.on('pageerror', (e) => console.log(`[${name} error]`, e.message));
    await page.goto(`${origin}/${name}`);
    return page;
  };
  const join = async (page, path, make) => {
    const offer = withoutMdns(await page.evaluate(make));
    const res = await api('POST', path, offer);
    if (res.status !== 200) throw new Error(`${path}: ${res.status} ${res.text}`);
    await page.evaluate((sdp) => window.answer(sdp), res.text);
  };

  const alice = await open('alice');
  await join(alice, `/rooms/${room}/publish/alice`, () => window.publish());
  const bob = await open('bob');
  await join(bob, `/rooms/${room}/subscribe/bob/alice`, () => window.subscribe());

  const t0 = Date.now();
  let a, b;
  for (;;) {
    [a, b] = [await alice.evaluate(() => window.numbers()), await bob.evaluate(() => window.numbers())];
    if (a.state === 'connected' && b.state === 'connected') break;
    if (a.state === 'failed' || b.state === 'failed' || Date.now() - t0 > 15000) break;
    await sleep(200);
  }
  summary.connectedAfterMs = Date.now() - t0;
  console.log(`alice ${a.state}, bob ${b.state} after ${summary.connectedAfterMs} ms`);
  if (a.state === 'connected' && b.state === 'connected') await sleep(seconds * 1000);
  [a, b] = [await alice.evaluate(() => window.numbers()), await bob.evaluate(() => window.numbers())];
  const room_ = await fetch(`http://${http}/rooms/${room}`, { headers: { Authorization: `Bearer ${secret}` } }).then((r) => r.text());
  Object.assign(summary, { alice: a, bob: b, room: JSON.parse(room_) });
  console.log(JSON.stringify({ alice: a, bob: b }, null, 2));

  const checks = [
    ['alice (publisher) connected', a.state === 'connected'],
    ['bob (subscriber) connected', b.state === 'connected'],
    ['alice sends H.264', a.outVideo === 'video/H264'],
    ['alice encoded > 100 frames', a.framesEncoded > 100],
    ['alice was asked for a keyframe (PLI) through the server', a.pliReceived >= 1],
    ['bob receives H.264', b.inVideo === 'video/H264'],
    ['bob decoded > 100 frames', b.framesDecoded > 100],
    ['bob decoded a keyframe', b.keyFramesDecoded >= 1],
    ['bob receives Opus', b.inAudio === 'audio/opus'],
    ['bob received > 100 audio packets', b.audioReceived > 100],
  ];
  console.log('\n--- checks ---');
  ok = true;
  for (const [name, pass] of checks) {
    console.log(`${pass ? 'PASS' : 'FAIL'}  ${name}`);
    if (!pass) ok = false;
  }
} catch (e) {
  console.log('error:', e.message);
  summary.error = e.message;
} finally {
  if (browser) await browser.close();
}
console.log(JSON.stringify({ ok, ...summary }));
process.exit(ok ? 0 : 1);
