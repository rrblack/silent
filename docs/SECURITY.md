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
    | ==== sealed Hello{name} ===================================================> |
    | <=================================================== sealed Hello{name} ==== |
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

- the protocol version
- the nameplate
- the two peer ids

So a transcript can't be replayed into another room or between a different pair of peers.

HKDF-SHA256 then derives three keys from the SPAKE2 output:

- one sealing key per direction (`lo->hi` and `hi->lo`)
- a safety-word key

### Sealed signaling

All signaling after SPAKE2 goes through the relay as ChaCha20-Poly1305 envelopes:

- **Nonce:** a 64-bit counter per direction.
- **Associated data:** the protocol label plus the sender and receiver peer ids.
- **Ordering:** the receiver accepts only the exact next counter. Replayed, reordered or dropped envelopes are rejected.

The signaling carried this way covers display names, SDP (which includes the DTLS certificate fingerprints) and ICE candidates.

**A mistyped code** gives different pair keys, so the first sealed `Hello` fails to open. The peers mark each other as *wrong code* and stop talking to each other. An attacker without the code gets exactly one online guess per handshake, and the relay never sees anything that would allow offline guessing.

### Media and chat

WebRTC's DTLS uses the certificate fingerprints from the sealed SDP, so the DTLS handshake is authenticated end to end:

- **Media:** DTLS-SRTP with ephemeral ECDHE keys, which gives forward secrecy.
- **Chat and media state:** a DTLS data channel.

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

1. **Room squatting:** anyone can hold a slot by joining a room number with a wrong code. They can't read or send anything, but they count toward the limit of 4. A future version could let verified members ask the relay to evict peers that failed verification with all of them.
2. **Online guessing:** 24 bits of secret plus one guess per handshake makes a successful guess against a live room negligible: 1 in 16.7 million per attempt, and every failed attempt is announced in the room. Codes are not meant to be reused.
3. **Endpoints:** a compromised computer, or a friend who screen-records, defeats any protocol. "Nothing is recorded" describes the relay and the app, not the people in the call.
4. **Supply chain:** builds and auto-updates must be signed. This is not set up yet (see HANDOFF.md).
5. **Metadata:** the relay and TURN provider (Cloudflare when using the Worker and Cloudflare TURN) see IP addresses and timing. Self-hosting `silent-relay` and coturn removes that third party.
6. **No audit yet:** the protocol uses standard primitives from well-reviewed crates, but neither the composition nor the code has had an independent review.
