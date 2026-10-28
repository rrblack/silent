# Silent: protocol and threat model

## Goals

- Only the people in a room can see or hear anything said in it: audio, video, screen, chat and display names.
- The relay can't read anything, and it can't sit in the middle of a call, even if it is malicious.
- No accounts, no stored history. A room exists only while someone is in it.

## How a room works

```
  Alice                         Relay (Worker / silent-relay)                 Bob
    | -- WS /v1/rooms/4071?intent=create ----> |                               |
    | <-- welcome {you:1, peers:[], ice} ------ |                               |
    |                                           | <-- WS /v1/rooms/4071?join -- |
    | <-- joined {peer:2} --------------------- | -- welcome {you:2,peers:[1]}->|
    | ==== SPAKE2(code) msg, cleartext ========================================> |
    | <=================================================== SPAKE2(code) msg ==== |
    |        both derive pair keys (only equal if both typed the same code)      |
    | ==== sealed Hello{name, session} ==========================================> |
    | <========================================== sealed Hello{name, session} ==== |
    | <=================================================== sealed Offer{SDP} ==== |
    | ==== sealed Answer{SDP} ===================================================> |
    | <=========================== sealed ICE candidates ==========================> |
    |                                                                             |
    | <====== DTLS-SRTP media + DTLS/SCTP chat, direct or via TURN ==============> |
```

### Invite code

The code looks like `4071-amber-falcon-river`.

- **`4071`** is the *nameplate*. It is the only part the relay sees, and it only routes peers into the same room.
- **`amber-falcon-river`** is the secret: 3 words from a 256-word list, which is 24 bits. It never leaves the device. It's used only as the SPAKE2 password, together with the nameplate.

### Pairwise key exchange

Every pair of peers runs symmetric SPAKE2 (Ed25519 group, `spake2` crate). The SPAKE2 identity binds three things:

- the handshake label (`silent/v1`)
- the nameplate
- the two peer ids

So a transcript can't be replayed into another room or between a different pair of peers. The app protocol version (`PROTOCOL_VERSION`) travels in the sealed `Hello`; when two builds disagree, a build of protocol 5 or later stops talking to the other and says the friend has a different version of Silent (older builds just never finish connecting).

HKDF-SHA256 then derives three keys from the SPAKE2 output:

- one sealing key per direction (`lo->hi` and `hi->lo`)
- a safety-word key

### Sealed signaling

All signaling after SPAKE2 goes through the relay as ChaCha20-Poly1305 envelopes:

- **Nonce:** a 64-bit counter per direction.
- **Associated data:** the protocol label plus the sender and receiver peer ids.
- **Ordering:** the receiver accepts only the exact next counter. Replayed, reordered or dropped envelopes are rejected.

The signaling carried this way covers display names, session ids, codec wishes (whether a device would like to send H.264), SDP (which includes the DTLS certificate fingerprints), ICE candidates, and requests to restart ICE.

**A mistyped code** gives different pair keys, so the first sealed `Hello` fails to open. The peers mark each other as *wrong code* and stop talking to each other. An attacker without the code gets exactly one online guess per handshake, and the relay never sees anything that would allow offline guessing.

**One guess per handshake, and handshakes are budgeted on the device too.** The guess is checked against the sealed `Hello` a device sends at the end of each handshake, so an attacker can test it and leave without sending anything back: such an attempt looks like someone who joined and left, not like a wrong code. The relays budget join attempts per room, but a malicious relay could make up joiners as fast as it likes. So every app keeps the same budget for itself (`JOIN_BURST` handshakes with people who haven't verified yet, then one every `JOIN_REFILL_SECS`; a friend who verifies gives their attempt back) and ignores newcomers once it is spent.

### Media and chat

WebRTC's DTLS uses the certificate fingerprints from the sealed SDP, so the DTLS handshake is authenticated end to end:

- **Media:** DTLS-SRTP with ephemeral ECDHE keys, which gives forward secrecy. Each connection carries the voice, camera, screen, and (since protocol 4) a separate stream for computer audio shared along with the screen.
- **Chat and media state:** a DTLS data channel (negotiated id 0). Since protocol 5 it also carries how big each person shows a friend's camera (width and height in pixels), so the friend encodes no larger than that; it reveals only a tile size, and only to that friend, though its changes over time hint at layout changes (a resized window, someone else spotlighted, picture-in-picture).
- **Files:** a second DTLS data channel (id 1), so a transfer never holds up chat. A file is a `Start` frame (name, size) followed by chunks, on an ordered, reliable channel. Receivers accept at most 64 MiB per file and 4 transfers per friend at a time, strip directories and control characters from names, and keep files in memory only; nothing is written to disk unless the user saves a file somewhere.

### Reconnecting

- **A connection that drops** is ICE-restarted by the side that sent the original offer: a new sealed offer with fresh ICE credentials. The other side can ask for one with a sealed `Restart`. DTLS keys and fingerprints don't change, so the safety words stay the same.
- **When the relay connection drops**, calls keep going over their direct connections, and the app rejoins the same nameplate (recreating the room on the relay if it is empty). The relay hands out a new peer id, so every pair runs SPAKE2 again from scratch and gets new keys and new safety words.
- **The session id** in `Hello` is 16 random bytes a device picks for each room. It lets friends recognise a device that came back under a new peer id and replace its old connection. It is sealed, so the relay never sees it. Anyone who holds the code could copy someone's session id to take over their place in the friend list, but they could equally just pick that person's name; comparing safety words is still the way to tell devices apart.

### Safety words

Each pair can compare four words, derived with HKDF from the pair's safety-word key and both DTLS fingerprints. Matching words mean the PAKE and the media connection both run between the same two devices. Someone who learned the code and sat in the middle would end up with different keys on each side, so the words would differ.

## What each party can learn

| Party | Can see | Cannot see |
|---|---|---|
| Relay operator | Nameplate, when peers connect and disconnect, how many are in a room, IP addresses of WebSocket clients, sizes and timing of sealed messages | Names, chat, audio, video, screen, SDP, ICE candidates, the secret words |
| TURN relay (when used) | IP addresses of both ends, traffic volume and timing | Any media or chat content (DTLS-SRTP) |
| STUN servers | Your public IP address | Anything else |
| Network observers | That you are talking to a relay, TURN or peer IP; traffic volume and timing | Content |
| Other people in the room | Everything shared in the room, **your IP address** unless "Hide my IP" is on (forces TURN) | Anything from other rooms |

## Known limitations

1. **Room squatting:** anyone can join a room number with a wrong code. They can't read or send anything, and they no longer keep their slot: when a pair's key exchange fails, each app asks the relay to remove the other (`evict`). The relay removes peer X only once **every peer that joined before X** has asked, so the people already in a room can remove a newcomer, but a newcomer can never remove them. What's left: a squatter alone in a room with someone who joined after them can have that person removed, and a squatter can rejoin until the join budget runs out.
2. **Online guessing:** 24 bits of secret plus one guess per handshake makes a successful guess against a live room unlikely: 1 in 16.7 million per attempt. A guesser who answers with a wrong `Hello` is announced as a wrong code; one who leaves after reading ours just shows up as someone joining and leaving. Both relays budget join attempts per room, and each app budgets handshakes the same way (see above): 20 at once, then one every 6 seconds (`JOIN_BURST`, `JOIN_REFILL_SECS`), which caps a guesser, even a malicious relay, at about 14,000 attempts a day per device, or roughly a 1 in 1,200 chance per day that a room stays open. The flip side is that someone spamming joins can keep friends who dropped off from rejoining for a while; calls already running are unaffected. Codes are not meant to be reused.
3. **Endpoints:** a compromised computer, or a friend who screen-records, defeats any protocol. "Nothing is recorded" describes the relay and the app, not the people in the call.
4. **Supply chain:** builds and auto-updates must be signed. This is not set up yet (see HANDOFF.md).
5. **Metadata:** the relay and TURN provider (Cloudflare when using the Worker and Cloudflare TURN) see IP addresses and timing. Self-hosting `silent-relay` and coturn removes that third party.
6. **No audit yet:** the protocol uses standard primitives from well-reviewed crates, but neither the composition nor the code has had an independent review.
