// Chrome <-> str0m interop probe for the tinyrtc profile. Starts the peer (target/release),
// opens its page in the installed Chrome (headless, fake camera + mic), lets the call run, then
// asserts on both sides' numbers. Run with node (Playwright hangs under Bun on this machine):
//
//   node interop.mjs [--window 10] [--headed] [--host 127.0.0.1] [--loss N] [--all-h264]
//                    [--core [--chrome-answers] [--restart] [--relay]]
//
// --all-h264: the browser offers every H.264 variant it has, not only Constrained Baseline
// packetization-mode=1, to see which one the peer's answer picks.
// --loss N: the peer drops every Nth echoed video packet and all its RTX, so the browser
// has to ask for keyframes; adds the assertions for the keyframe-request forwarding path
// (the raw peer only: the core owns its socket).
// --core: the peer is the library itself (src/bin/tinyrtc-core-peer.rs: a `Connection` and its
// own SDP), not raw str0m. --chrome-answers: the core offers and Chrome answers (the page's
// ?role=answer); without it Chrome offers, as with the raw peer. --restart (with --core): 3 s
// after connecting, the side that offered restarts ICE; the call must go on through it.
// --relay (with --core): Chrome is given the core's address only as the media server's relay
// candidate — `typ relay`, priority 255, through addIceCandidate (the descriptions carry none) —
// and must connect through it: its selected pair's remote candidate is that relay one.
// The peer's stderr, both descriptions in it, goes to target/interop-peer.log (raw),
// target/interop-core.log (--core) or target/interop-core-answer.log (--core --chrome-answers);
// -relay before .log with --relay.
// Exit code 0 = every assertion held.

import { spawn, spawnSync } from 'node:child_process';
import { createInterface } from 'node:readline';
import { createWriteStream, existsSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import { chromium } from 'playwright';

const here = dirname(fileURLToPath(import.meta.url));
const argv = process.argv.slice(2);
const opt = (name, dflt) => {
  const i = argv.indexOf('--' + name);
  return i >= 0 ? argv[i + 1] : dflt;
};
const windowSeconds = Number(opt('window', '10'));
const host = opt('host', '127.0.0.1');
const headed = argv.includes('--headed');
const loss = Number(opt('loss', '0'));
const allH264 = argv.includes('--all-h264');
const core = argv.includes('--core');
const chromeAnswers = argv.includes('--chrome-answers');
const restart = argv.includes('--restart');
const relay = argv.includes('--relay');
if ((restart || relay) && !core) {
  console.error('interop: --restart and --relay need --core');
  process.exit(2);
}
if (chromeAnswers && !core) {
  console.error('interop: --chrome-answers needs --core (the raw peer only answers)');
  process.exit(2);
}
if (loss && core) {
  console.error('interop: --loss needs the raw peer (the core owns its socket)');
  process.exit(2);
}
const connectTimeout = 15;
// The peer runs on its own clock: page load + connect + the window + slack, then it exits and
// prints its final line.
const peerSeconds = connectTimeout + windowSeconds + 6;

const bin = core ? 'tinyrtc-core-peer' : 'tinyrtc-peer';
const exe = join(here, 'target', 'release', process.platform === 'win32' ? bin + '.exe' : bin);
const build = spawnSync('cargo', ['build', '--release', '-q'], { cwd: here, stdio: 'inherit' });
if (build.status !== 0 || !existsSync(exe)) {
  console.error('interop: cargo build failed');
  process.exit(2);
}

// The peer's stderr (its diagnostics + both SDPs) also goes to the log.
const logName = (!core ? 'interop-peer' : chromeAnswers ? 'interop-core-answer' : 'interop-core') + (relay ? '-relay' : '') + '.log';
const logPath = join(here, 'target', logName);
const logFile = createWriteStream(logPath);
const peerArgs = ['--host', host, '--seconds', String(peerSeconds), ...(loss ? ['--drop-video', String(loss)] : []), ...(relay ? ['--relay'] : [])];
const peer = spawn(exe, peerArgs, { cwd: here, stdio: ['ignore', 'pipe', 'pipe'] });
const peerLog = [];
peer.stderr.on('data', (d) => { peerLog.push(d.toString()); logFile.write(d); });
let listening = null;
let lastStats = null;
let finalStats = null;
const listeningP = new Promise((resolve) => {
  createInterface({ input: peer.stdout }).on('line', (line) => {
    let j;
    try { j = JSON.parse(line); } catch { console.log('[peer]', line); return; }
    if (j.type === 'listening') { listening = j; resolve(j); }
    else if (j.type === 'stats') lastStats = j;
    else if (j.type === 'final') finalStats = j;
  });
});
const peerExit = new Promise((resolve) => peer.on('exit', (code) => resolve(code)));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let browser;
let failed = false;
try {
  const l = await Promise.race([listeningP, sleep(10000).then(() => null)]);
  if (!l) throw new Error('the peer did not start listening');
  const who = chromeAnswers ? 'the peer offers, Chrome answers' : 'Chrome offers, the peer answers';
  console.log(`peer: ${core ? 'the core' : 'raw str0m'} at ${l.url} (udp ${l.udp}), exits after ${l.seconds} s; ${who}`);

  browser = await chromium.launch({
    channel: 'chrome',
    headless: !headed,
    args: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
      // Chrome gathers no loopback candidate by default, and its LAN host candidates are never
      // paired with the peer's 127.0.0.1: without this switch nothing connects on loopback.
      ...(host.startsWith('127.') ? ['--allow-loopback-in-peer-connection'] : []),
    ],
  });
  const context = await browser.newContext();
  await context.grantPermissions(['camera', 'microphone'], { origin: l.url.replace(/\/$/, '') });
  const page = await context.newPage();
  page.on('console', (m) => { if (m.type() === 'error' || m.type() === 'warning') console.log(`[page ${m.type()}]`, m.text()); });
  page.on('pageerror', (e) => console.log('[page error]', e.message));
  const query = [allH264 ? 'h264=all' : '', chromeAnswers ? 'role=answer' : '', restart ? 'restart=1' : '', relay ? 'relay=1' : '']
    .filter(Boolean).join('&');
  await page.goto(l.url + (query ? '?' + query : ''));

  // Connected (or failed) within connectTimeout.
  const t0 = Date.now();
  let s;
  for (;;) {
    s = await page.evaluate(() => window.__stats);
    if (s.error || s.connectionState === 'connected' || s.connectionState === 'failed') break;
    if (Date.now() - t0 > connectTimeout * 1000) break;
    await sleep(200);
  }
  if (s.error) throw new Error('page: ' + s.error);
  console.log(`browser: ${s.connectionState} after ${((Date.now() - t0) / 1000).toFixed(1)} s`);
  if (s.connectionState === 'connected') await sleep(windowSeconds * 1000);
  s = await page.evaluate(() => window.__stats);
  const peerAtRead = lastStats;

  const exited = await Promise.race([peerExit, sleep((peerSeconds + 10) * 1000).then(() => 'timeout')]);
  const p = finalStats || peerAtRead || {};

  console.log(`\n--- offer SDP (${chromeAnswers ? 'the peer' : 'Chrome'}), video m-section ---`);
  console.log((s.offerVideoSection || ['(none)']).join('\n'));
  console.log(`\n--- answer SDP (${chromeAnswers ? 'Chrome' : 'the peer'}), video m-section ---`);
  console.log((s.answerVideoSection || ['(none)']).join('\n'));
  console.log('\n--- browser (window.__stats) ---');
  const { offerSdp, answerSdp, answerVideoSection, offerVideoSection, ...browserNumbers } = s;
  console.log(JSON.stringify(browserNumbers, null, 2));
  console.log('\n--- peer at the browser read ---');
  console.log(JSON.stringify(peerAtRead));
  console.log('\n--- peer final ---');
  console.log(JSON.stringify(finalStats), `(exit ${exited})`);

  const checks = [
    ['browser connectionState is connected', s.connectionState === 'connected'],
    ['peer connected', p.connected === true],
    ['outbound video codec is video/H264', s.outboundVideoCodec === 'video/H264'],
    ['inbound video codec is video/H264', s.inboundVideoCodec === 'video/H264'],
    ['browser framesEncoded > 100', s.framesEncoded > 100],
    // Under --loss the echo is broken on purpose: the decoder stalls from each unrepaired loss
    // to the next keyframe, so only "it decodes at all" is asked.
    loss
      ? ['browser framesDecoded > 0 (the echo decodes; stalls under --loss)', s.framesDecoded > 0]
      : ['browser framesDecoded > 100 (the echo decodes)', s.framesDecoded > 100],
    ['browser inbound audio is audio/opus', s.inboundAudioCodec === 'audio/opus'],
    ['browser inbound audio packetsReceived > 0', s.audioPacketsReceived > 0],
    ['peer video frames > 0', p.video_frames > 0],
    ['peer video keyframes >= 1', p.video_keyframes >= 1],
    ['peer audio frames > 0', p.audio_frames > 0],
    ['peer channel messages > 0', p.channel_messages > 0],
    ['browser got channel echoes', s.channelEchoes > 0 && s.messagesReceived > 0],
    // The browser saw the tracks (a=msid) and its <video> presents the echo.
    ['browser raised track events for audio and video', s.trackKinds.includes('audio') && s.trackKinds.includes('video')],
    ['the remote <video> presented frames', s.remoteVideoFrames > 10 && s.remoteVideoWidth > 0],
  ];
  if (restart) {
    const u = s.restartUfrags;
    checks.push(
      ['restart: done', s.restarted === true],
      ['restart: new ICE credentials both ways', !!u && u.before[0] !== u.after[0] && u.before[1] !== u.after[1]],
      ['restart: the echo kept decoding after it (> 100 frames)', s.framesDecoded - s.framesDecodedAtRestart > 100],
      ['restart: channel echoes after it', s.channelEchoes - s.channelEchoesAtRestart > 10],
    );
  }
  if (relay) {
    const r = s.selectedRemote;
    const lines = s.relayLines || [];
    console.log('\n--- relay: the lines added, the remote candidates the browser knows, the selected one ---');
    console.log(JSON.stringify({ lines, remoteCandidates: s.remoteCandidates, selected: r }, null, 2));
    checks.push(
      ['relay: the descriptions the browser got carry no candidate', !/a=candidate/.test((chromeAnswers ? s.offerSdp : s.answerSdp) || '')],
      ['relay: the browser took the typ relay line (addIceCandidate resolved)', lines.length >= (restart ? 2 : 1) && lines.every((l) => / typ relay raddr 0\.0\.0\.0 rport 0$/.test(l))],
      ['relay: the selected pair\'s remote candidate is that relay one (priority 255)', r?.type === 'relay' && r?.priority === 255],
      ['relay: the browser knows the peer by that candidate only', s.remoteCandidates.length > 0 && s.remoteCandidates.every((c) => c.type === 'relay' && c.port === r?.port)],
    );
  }
  if (loss) {
    checks.push(
      ['loss: the peer dropped video packets', p.video_packets_dropped > 0],
      ['loss: the browser asked the peer for keyframes (PLI/FIR sent)', s.pliSent + s.firSent > 0],
      ['loss: the peer received keyframe requests', p.keyframe_requests_received > 0],
      ['loss: the peer forwarded them (requests sent > the start one)', p.keyframe_requests_sent > 1],
      ['loss: the browser encoder got them (pliReceived > 1)', s.pliReceived + s.firReceived > 1],
      ['loss: new keyframes reached the peer', p.video_keyframes >= 2],
      ['loss: the browser decoded more than one keyframe', s.keyFramesDecoded >= 2],
    );
  }
  console.log('\n--- assertions ---');
  for (const [name, ok] of checks) {
    console.log(`${ok ? 'PASS' : 'FAIL'}  ${name}`);
    if (!ok) failed = true;
  }
} catch (e) {
  console.error('interop: ' + (e && e.stack || e));
  failed = true;
} finally {
  if (browser) await browser.close().catch(() => {});
  if (peer.exitCode === null) peer.kill();
}
logFile.end();
if (failed) {
  console.log('\n--- peer stderr (tail; all of it in target/interop-peer.log) ---');
  console.log(peerLog.join('').split('\n').slice(-60).join('\n'));
}
console.log(failed ? '\nINTEROP: FAIL' : '\nINTEROP: PASS');
process.exit(failed ? 1 : 0);
