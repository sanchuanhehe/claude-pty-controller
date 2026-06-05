// Reference dashboard (Node CLI). Connects to the relay, runs the Noise XXpsk3
// handshake with the controller, decrypts the three channels, prints one JSON
// line per normalized message ("MSG …"), and forwards stdin lines as encrypted
// `input`. Proves the dashboard contract is implementable outside Rust.
//
// Usage: PAIRING_SECRET=… RELAY_TOKEN=… node dashboard/cli.mjs --url ws://relay:9000
import { Initiator, genKeypair, derivePsk, roomIdNow } from './noise.mjs';

const arg = (n, d) => { const i = process.argv.indexOf(n); return i >= 0 ? process.argv[i + 1] : d; };
const url = arg('--url', 'ws://127.0.0.1:9000');
const pairing = arg('--pairing', process.env.PAIRING_SECRET);
const relayToken = arg('--relay-token', process.env.RELAY_TOKEN || null);
const peer = arg('--peer', 'dash-cli');
const room = arg('--room', roomIdNow(pairing));

if (!pairing) { console.error('need --pairing or PAIRING_SECRET'); process.exit(2); }

const psk = derivePsk(pairing);
const me = genKeypair();
console.error(`[dash] pubkey=${me.pub.toString('base64')} room=${room}`);

const ws = new WebSocket(url);
const send = (o) => ws.send(JSON.stringify(o));
let hs = null;
let up = false;
const seen = new Set();
// Encrypt an inbound app message and send it as an opaque Msg to the controller.
const sendEnc = (obj) => send({ t: 'msg', to: 'controller', data: hs.encrypt(Buffer.from(JSON.stringify(obj), 'utf8')).toString('base64') });

ws.onopen = () => send({ t: 'join', room, role: 'dashboard', peer, token: relayToken });

ws.onmessage = (ev) => {
  const env = JSON.parse(ev.data);
  switch (env.t) {
    case 'joined':
      console.error('[dash] joined relay');
      break;
    case 'peer_join':
      if (env.peer === 'controller') {
        hs = new Initiator(me.priv, me.pub, psk);
        send({ t: 'msg', to: 'controller', data: hs.writeMsg1().toString('base64') });
      }
      break;
    case 'peer_leave':
      if (env.peer === 'controller') { up = false; hs = null; console.error('[dash] controller left'); }
      break;
    case 'deliver': {
      const bytes = Buffer.from(env.data, 'base64');
      if (!up) {
        hs.readMsg2(bytes);
        send({ t: 'msg', to: 'controller', data: hs.writeMsg3().toString('base64') });
        up = true;
        console.error(`[dash] E2EE up; controller pubkey=${hs.remoteStatic().toString('base64')}`);
        // Pull transcript history (new/reconnecting dashboard) — §3.2 refresh:full.
        sendEnc({ type: 'refresh', scope: 'full' });
      } else {
        try { onMessage(JSON.parse(hs.decrypt(bytes).toString('utf8'))); } catch (e) { console.error('[dash] decrypt/parse', e.message); }
      }
      break;
    }
    case 'error':
      console.error('[dash] relay error:', env.msg);
      break;
  }
};
ws.onclose = () => { console.error('[dash] closed'); process.exit(0); };
ws.onerror = (e) => console.error('[dash] ws error', e.message || e);

function onMessage(m) {
  if (m.type === 'transcript') {
    const k = `${m.msg_uuid}:${m.part_index}`;
    if (seen.has(k)) return; // dedup (msgUuid, partIndex)
    seen.add(k);
  }
  process.stdout.write('MSG ' + JSON.stringify(m) + '\n');
}

// stdin → encrypted input frames (one per line)
process.stdin.setEncoding('utf8');
process.stdin.on('data', (chunk) => {
  if (!up) return;
  for (const text of chunk.split('\n')) {
    if (text === '') continue;
    sendEnc({ type: 'input', text });
  }
});
