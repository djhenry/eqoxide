#!/usr/bin/env python3
"""
Mock EQ server for testing the client without a real EQEmu server.
Handles login → world → zone handshake and sends the client into "testzone".

Usage:
    python3 mock_eq_server.py [--login-port 5998] [--world-port 9000] [--zone-port 9000]
"""

import socket
import struct
import threading
import time
import argparse
import zlib
import random
import sys

# ── EQ Transport opcodes ──────────────────────────────────────────────────

OP_SESSION_REQUEST  = 0x01
OP_SESSION_RESPONSE = 0x02
OP_PACKET           = 0x09
OP_FRAGMENT         = 0x0d
OP_ACK              = 0x15
OP_KEEPALIVE        = 0x06

# ── EQ App opcodes ────────────────────────────────────────────────────────

OP_SESSION_READY       = 0x0001
OP_LOGIN               = 0x0002
OP_SERVER_LIST_REQUEST = 0x0004
OP_PLAY_EVERQUEST_REQ  = 0x000d
OP_CHAT_MESSAGE        = 0x0016
OP_LOGIN_ACCEPTED      = 0x0017
OP_SERVER_LIST_RESPONSE= 0x0018
OP_PLAY_EVERQUEST_RESP = 0x0021

OP_SEND_LOGIN_INFO     = 0x4dd0
OP_SEND_CHAR_INFO      = 0x4513
OP_ENTER_WORLD         = 0x7cba
OP_POST_ENTER_WORLD    = 0x52a4
OP_ZONE_SERVER_INFO    = 0x61b6

OP_ZONE_ENTRY          = 0x7213
OP_NEW_ZONE            = 0x0920
OP_REQ_CLIENT_SPAWN    = 0x0322
OP_WEATHER             = 0x254d
OP_SEND_EXP_ZONE_IN    = 0x0587
OP_CLIENT_READY        = 0x5e20
OP_REQ_NEW_ZONE        = 0x7ac5

OP_NEW_SPAWN           = 0x1860
OP_ZONE_SPAWNS         = 0x2e78
OP_PLAYER_PROFILE      = 0x75df
OP_TIME_OF_DAY         = 0x1580
OP_CHAR_INVENTORY      = 0x5394
OP_SET_SERVER_FILTER   = 0x6563
OP_APPROVE_WORLD       = 0x3c25
OP_LOG_SERVER          = 0x0fa6
OP_EXPANSION_INFO      = 0x04ec
OP_WORLD_COMPLETE      = 0x509d
OP_WORLD_CLIENT_READY  = 0x5e99

# ── CRC32 (EQ-style) ─────────────────────────────────────────────────────

CRC32_TABLE = [
    0x00000000, 0x77073096, 0xEE0E612C, 0x990951BA, 0x076DC419, 0x706AF48F, 0xE963A535, 0x9E6495A3,
    0x0EDB8832, 0x79DCB8A4, 0xE0D5E91E, 0x97D2D988, 0x09B64C2B, 0x7EB17CBD, 0xE7B82D07, 0x90BF1D91,
    0x1DB71064, 0x6AB020F2, 0xF3B97148, 0x84BE41DE, 0x1ADAD47D, 0x6DDDE4EB, 0xF4D4B551, 0x83D385C7,
    0x136C9856, 0x646BA8C0, 0xFD62F97A, 0x8A65C9EC, 0x14015C4F, 0x63066CD9, 0xFA0F3D63, 0x8D080DF5,
    0x3B6E20C8, 0x4C69105E, 0xD56041E4, 0xA2677172, 0x3C03E4D1, 0x4B04D447, 0xD20D85FD, 0xA50AB56B,
    0x35B5A8FA, 0x42B2986C, 0xDBBBC9D6, 0xACBCF940, 0x32D86CE3, 0x45DF5C75, 0xDCD60DCF, 0xABD13D59,
    0x26D930AC, 0x51DE003A, 0xC8D75180, 0xBFD06116, 0x21B4F4B5, 0x56B3C423, 0xCFBA9599, 0xB8BDA50F,
    0x2802B89E, 0x5F058808, 0xC60CD9B2, 0xB10BE924, 0x2F6F7C87, 0x58684C11, 0xC1611DAB, 0xB6662D3D,
    0x76DC4190, 0x01DB7106, 0x98D220BC, 0xEFD5102A, 0x71B18589, 0x06B6B51F, 0x9FBFE4A5, 0xE8B8D433,
    0x7807C9A2, 0x0F00F934, 0x9609A88E, 0xE10E9818, 0x7F6A0DBB, 0x086D3D2D, 0x91646C97, 0xE6635C01,
    0x6B6B51F4, 0x1C6C6162, 0x856530D8, 0xF262004E, 0x6C0695ED, 0x1B01A57B, 0x8208F4C1, 0xF50FC457,
    0x65B0D9C6, 0x12B7E950, 0x8BBEB8EA, 0xFCB9887C, 0x62DD1DDF, 0x15DA2D49, 0x8CD37CF3, 0xFBD44C65,
    0x4DB26158, 0x3AB551CE, 0xA3BC0074, 0xD4BB30E2, 0x4ADFA541, 0x3DD895D7, 0xA4D1C46D, 0xD3D6F4FB,
    0x4369E96A, 0x346ED9FC, 0xAD678846, 0xDA60B8D0, 0x44042D73, 0x33031DE5, 0xAA0A4C5F, 0xDD0D7CC9,
    0x5005713C, 0x270241AA, 0xBE0B1010, 0xC90C2086, 0x5768B525, 0x206F85B3, 0xB966D409, 0xCE61E49F,
    0x5EDEF90E, 0x29D9C998, 0xB0D09822, 0xC7D7A8B4, 0x59B33D17, 0x2EB40D81, 0xB7BD5C3B, 0xC0BA6CAD,
    0xEDB88320, 0x9ABFB3B6, 0x03B6E20C, 0x74B1D29A, 0xEAD54739, 0x9DD277AF, 0x04DB2615, 0x73DC1683,
    0xE3630B12, 0x94643B84, 0x0D6D6A3E, 0x7A6A5AA8, 0xE40ECF0B, 0x9309FF9D, 0x0A00AE27, 0x7D079EB1,
    0xF00F9344, 0x8708A3D2, 0x1E01F268, 0x6906C2FE, 0xF762575D, 0x806567CB, 0x196C3671, 0x6E6B06E7,
    0xFED41B76, 0x89D32BE0, 0x10DA7A5A, 0x67DD4ACC, 0xF9B9DF6F, 0x8EBEEFF9, 0x17B7BE43, 0x60B08ED5,
    0xD6D6A3E8, 0xA1D1937E, 0x38D8C2C4, 0x4FDFF252, 0xD1BB67F1, 0xA6BC5767, 0x3FB506DD, 0x48B2364B,
    0xD80D2BDA, 0xAF0A1B4C, 0x36034AF6, 0x41047A60, 0xDF60EFC3, 0xA867DF55, 0x316E8EEF, 0x4669BE79,
    0xCB61B38C, 0xBC66831A, 0x256FD2A0, 0x5268E236, 0xCC0C7795, 0xBB0B4703, 0x220216B9, 0x5505262F,
    0xC5BA3BBE, 0xB2BD0B28, 0x2BB45A92, 0x5CB36A04, 0xC2D7FFA7, 0xB5D0CF31, 0x2CD99E8B, 0x5BDEAE1D,
    0x9B64C2B0, 0xEC63F226, 0x756AA39C, 0x026D930A, 0x9C0906A9, 0xEB0E363F, 0x72076785, 0x05005713,
    0x95BF4A82, 0xE2B87A14, 0x7BB12BAE, 0x0CB61B38, 0x92D28E9B, 0xE5D5BE0D, 0x7CDCEFB7, 0x0BDBDF21,
    0x86D3D2D4, 0xF1D4E242, 0x68DDB3F8, 0x1FDA836E, 0x81BE16CD, 0xF6B9265B, 0x6FB077E1, 0x18B74777,
    0x88085AE6, 0xFF0F6A70, 0x66063BCA, 0x11010B5C, 0x8F659EFF, 0xF862AE69, 0x616BFFD3, 0x166CCF45,
    0xA00AE278, 0xD70DD2EE, 0x4E048354, 0x3903B3C2, 0xA7672661, 0xD06016F7, 0x4969474D, 0x3E6E77DB,
    0xAED16A4A, 0xD9D65ADC, 0x40DF0B66, 0x37D83BF0, 0xA9BCAE53, 0xDEBB9EC5, 0x47B2CF7F, 0x30B5FFE9,
    0xBDBDF21C, 0xCABAC28A, 0x53B39330, 0x24B4A3A6, 0xBAD03605, 0xCDD70693, 0x54DE5729, 0x23D967BF,
    0xB3667A2E, 0xC4614AB8, 0x5D681B02, 0x2A6F2B94, 0xB40BBE37, 0xC30C8EA1, 0x5A05DF1B, 0x2D02EF8D,
]

def eq_crc32(data, key=0):
    key = key & 0xFFFFFFFF
    crc = 0xFFFFFFFF
    for i in range(4):
        b = (key >> (i * 8)) & 0xFF
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ b) & 0xFF)]
    for byte in data:
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ byte) & 0xFF)]
    return (~crc) & 0xFFFFFFFF


# ── Mock EQ Session ───────────────────────────────────────────────────────

class MockEQSession:
    """Handles the EQ transport protocol for one connection."""

    def __init__(self, sock, addr, encode_key=0):
        self.sock = sock
        self.addr = addr
        self.encode_key = encode_key
        self.crc_bytes = 0
        self.send_seq = 0
        self.connected = False

    def next_seq(self):
        s = self.send_seq
        self.send_seq = (self.send_seq + 1) & 0xFFFF
        return s

    def send_raw(self, opcode, payload):
        raw = bytes([0x00, opcode]) + payload
        self.sock.sendto(raw, self.addr)

    def send_app_packet(self, opcode, payload):
        app_data = struct.pack('<H', opcode) + payload
        self.send_reliable(app_data)

    def send_reliable(self, app_data):
        seq = self.next_seq()
        inner = struct.pack('>H', seq) + app_data
        self.send_raw(OP_PACKET, inner)

    def handle_session_request(self, data):
        """Handle OP_SESSION_REQUEST and respond with OP_SESSION_RESPONSE."""
        if len(data) < 12:
            return
        protocol_ver = struct.unpack('>I', data[0:4])[0]
        connect_code = struct.unpack('>I', data[4:8])[0]
        max_size = struct.unpack('>I', data[8:12])[0]

        self.connected = True
        self.encode_key = random.randint(1, 0x7FFFFFFF)

        # ReliableStreamConnectReply: connect_code(4) encode_key(4) crc(1) enc1(1) enc2(1) max_size(4)
        reply = struct.pack('>I', connect_code)
        reply += struct.pack('>I', self.encode_key)
        reply += bytes([0, 0, 0])  # crc_bytes=0, encode_pass1=0, encode_pass2=0
        reply += struct.pack('>I', max_size)

        self.send_raw(OP_SESSION_RESPONSE, reply)

    def on_recv(self, data):
        if len(data) < 2:
            return
        opcode = data[1]
        payload = data[2:]

        if opcode == OP_SESSION_REQUEST:
            self.handle_session_request(payload)
            return

        if opcode == OP_ACK or opcode == OP_KEEPALIVE:
            return

        if opcode == OP_PACKET:
            self.handle_packet(payload)
        elif opcode == OP_FRAGMENT:
            # For simplicity, ignore fragments for now
            pass

    def handle_packet(self, data):
        if len(data) < 2:
            return
        seq = struct.unpack('>H', data[0:2])[0]
        app_data = data[2:]
        self.send_ack(seq)
        if len(app_data) >= 2:
            opcode = struct.unpack('<H', app_data[0:2])[0]
            payload = app_data[2:]
            return (opcode, payload)
        return None

    def send_ack(self, seq):
        self.send_raw(OP_ACK, struct.pack('>H', seq))


# ── Login Server ──────────────────────────────────────────────────────────

class LoginServer:
    def __init__(self, port, world_host, world_port, zone_port):
        self.port = port
        self.world_host = world_host
        self.world_port = world_port
        self.zone_port = zone_port
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(('127.0.0.1', port))
        self.sock.settimeout(0.5)
        self.running = False

    def start(self):
        self.running = True
        print(f"[login] Listening on 127.0.0.1:{self.port}")
        while self.running:
            try:
                data, addr = self.sock.recvfrom(4096)
            except socket.timeout:
                continue
            except OSError:
                break
            self.handle_packet(data, addr)

    def stop(self):
        self.running = False
        self.sock.close()

    def handle_packet(self, data, addr):
        if len(data) < 2:
            return
        opcode = data[1]
        payload = data[2:]

        if opcode == OP_SESSION_REQUEST:
            self.handle_session(data, addr)
            return

        # App-level packet
        if opcode == OP_PACKET and len(payload) >= 2:
            seq = struct.unpack('>H', payload[0:2])[0]
            app_data = payload[2:]
            if len(app_data) >= 2:
                app_opcode = struct.unpack('<H', app_data[0:2])[0]
                app_payload = app_data[2:]
                self.dispatch_app(app_opcode, app_payload, addr, seq)

    def handle_session(self, data, addr):
        """Respond to OP_SESSION_REQUEST with OP_SESSION_RESPONSE."""
        if len(data) < 14:
            return
        connect_code = struct.unpack('>I', data[6:10])[0]
        max_size = struct.unpack('>I', data[10:14])[0]

        encode_key = random.randint(1, 0x7FFFFFFF)
        reply = struct.pack('>I', connect_code)
        reply += struct.pack('>I', encode_key)
        reply += bytes([0, 0, 0])  # no CRC, no encoding
        reply += struct.pack('>I', max_size)

        raw = bytes([0x00, OP_SESSION_RESPONSE]) + reply
        self.sock.sendto(raw, addr)
        print(f"[login] Session established with {addr} (encode_key={encode_key})")
        self._sessions = getattr(self, '_sessions', {})
        self._sessions[addr] = encode_key

    def send_app(self, addr, opcode, payload, seq=0):
        app_data = struct.pack('<H', opcode) + payload
        inner = struct.pack('>H', seq) + app_data
        raw = bytes([0x00, OP_PACKET]) + inner
        self.sock.sendto(raw, addr)

    def send_ack(self, addr, seq):
        raw = bytes([0x00, OP_ACK]) + struct.pack('>H', seq)
        self.sock.sendto(raw, addr)

    def dispatch_app(self, opcode, payload, addr, seq):
        self.send_ack(addr, seq)
        print(f"[login] Got opcode 0x{opcode:04x} ({len(payload)} bytes)")

        if opcode == OP_SESSION_READY:
            # Client ready — send chat message (session ready indicator)
            chat_msg = b'\x00' * 4  # empty chat message
            self.send_app(addr, OP_CHAT_MESSAGE, chat_msg)
            print("[login] Sent OP_CHAT_MESSAGE (session ready)")

        elif opcode == OP_LOGIN:
            # Credentials received — send login accepted
            # Decrypted LoginReply: success(1) + unknown(7) + lsid(4) + key
            lsid = 42
            ls_key = "mockkey"
            # We need DES-encrypted block, but since client checks:
            #   if dec[0] == 0 → rejected, else → accepted
            # And the client does DES decrypt with zero key/iv...
            # Let's just send the raw format the client expects
            login_reply = bytes([1])  # success=1
            login_reply += bytes(7)   # padding
            login_reply += struct.pack('<i', lsid)  # lsid
            login_reply += ls_key.encode() + b'\x00'
            # Pad to 8-byte boundary
            while len(login_reply) % 8 != 0:
                login_reply += b'\x00'

            # Encrypt with zero key/iv (DES-CBC)
            try:
                from des import Des
                from cbc import Encryptor
                from des.cipher.block_padding import NoPadding
                enc = Encryptor(b'\x00' * 8, b'\x00' * 8)
                encrypted = enc.encrypt_padded(login_reply, NoPadding)
            except ImportError:
                # Fallback: use pycryptodome
                try:
                    from Crypto.Cipher import DES
                    cipher = DES.new(b'\x00' * 8, DES.MODE_CBC, b'\x00' * 8)
                    encrypted = cipher.encrypt(login_reply)
                except ImportError:
                    # Last resort: send unencrypted (client may handle gracefully)
                    encrypted = login_reply
                    print("[login] WARNING: No DES library, sending unencrypted login reply")

            header = struct.pack('<IbbI', 3, 0, 2, 0)  # LoginBaseMessage header
            self.send_app(addr, OP_LOGIN_ACCEPTED, header + encrypted)
            print(f"[login] Sent OP_LOGIN_ACCEPTED (lsid={lsid})")

        elif opcode == OP_SERVER_LIST_REQUEST:
            # Send server list with one world server entry
            world_host = self.world_host
            world_port = self.world_port
            server_id = 1
            server_name = "MockWorld"

            # Build server list response
            # LoginBaseMessage prefix (16 bytes)
            header = bytes(16)
            # Count
            body = struct.pack('<i', 1)
            # Entry: ip\0 + server_type(4) + server_id(4) + name\0 + ...
            body += world_host.encode() + b'\x00'
            body += struct.pack('<I', 0)  # server_type (0=normal)
            body += struct.pack('<I', server_id)
            body += server_name.encode() + b'\x00'

            self.send_app(addr, OP_SERVER_LIST_RESPONSE, header + body)
            print(f"[login] Sent server list: {server_name} @ {world_host}:{world_port}")

        elif opcode == OP_PLAY_EVERQUEST_REQ:
            # Client chose a server — respond with play response
            self.send_app(addr, OP_PLAY_EVERQUEST_RESP, b'\x00' * 4)
            print("[login] Sent OP_PLAY_EVERQUEST_RESP")


# ── World Server ──────────────────────────────────────────────────────────

class WorldServer:
    def __init__(self, port, zone_port, char_name="Aiquestbot"):
        self.port = port
        self.zone_port = zone_port
        self.char_name = char_name
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(('127.0.0.1', port))
        self.sock.settimeout(0.5)
        self.running = False
        self._sessions = {}

    def start(self):
        self.running = True
        print(f"[world] Listening on 127.0.0.1:{self.port}")
        while self.running:
            try:
                data, addr = self.sock.recvfrom(8192)
            except socket.timeout:
                continue
            except OSError:
                break
            self.handle_packet(data, addr)

    def stop(self):
        self.running = False
        self.sock.close()

    def handle_packet(self, data, addr):
        if len(data) < 2:
            return
        opcode = data[1]
        payload = data[2:]

        if opcode == OP_SESSION_REQUEST:
            self.handle_session(data, addr)
            return

        if opcode == OP_PACKET and len(payload) >= 2:
            seq = struct.unpack('>H', payload[0:2])[0]
            app_data = payload[2:]
            if len(app_data) >= 2:
                app_opcode = struct.unpack('<H', app_data[0:2])[0]
                app_payload = app_data[2:]
                self.dispatch_app(app_opcode, app_payload, addr, seq)

    def handle_session(self, data, addr):
        if len(data) < 14:
            return
        connect_code = struct.unpack('>I', data[6:10])[0]
        max_size = struct.unpack('>I', data[10:14])[0]

        encode_key = random.randint(1, 0x7FFFFFFF)
        reply = struct.pack('>I', connect_code)
        reply += struct.pack('>I', encode_key)
        reply += bytes([0, 0, 0])
        reply += struct.pack('>I', max_size)

        raw = bytes([0x00, OP_SESSION_RESPONSE]) + reply
        self.sock.sendto(raw, addr)
        self._sessions[addr] = encode_key
        print(f"[world] Session established with {addr}")

    def send_app(self, addr, opcode, payload, seq=0):
        app_data = struct.pack('<H', opcode) + payload
        inner = struct.pack('>H', seq) + app_data
        raw = bytes([0x00, OP_PACKET]) + inner
        self.sock.sendto(raw, addr)

    def send_ack(self, addr, seq):
        raw = bytes([0x00, OP_ACK]) + struct.pack('>H', seq)
        self.sock.sendto(raw, addr)

    def dispatch_app(self, opcode, payload, addr, seq):
        self.send_ack(addr, seq)
        print(f"[world] Got opcode 0x{opcode:04x} ({len(payload)} bytes)")

        if opcode == OP_SEND_LOGIN_INFO:
            # Client sent login info — send char info (character list)
            self.send_char_info(addr)

        elif opcode == OP_ENTER_WORLD:
            # Client wants to enter world — send zone server info
            self.send_zone_server_info(addr)

        elif opcode == OP_POST_ENTER_WORLD:
            pass  # Acknowledged, no response needed

    def send_char_info(self, addr):
        """Send OP_SEND_CHAR_INFO with a minimal character list."""
        # CharInfo_Struct: count(4) + entries...
        # Each entry has: name(64), race(4), class(4), level(4), ...
        # For simplicity, send a minimal but valid structure
        char_name = self.char_name.encode()
        name_field = char_name + b'\x00' * (64 - len(char_name))

        # Build minimal char info (just need it to not crash the client)
        # The actual structure is complex; we send enough to trigger send_enter_world
        info = struct.pack('<I', 1)  # count = 1
        info += name_field  # char_name
        info += struct.pack('<I', 1)  # unknown
        info += struct.pack('<I', 1)  # unknown
        info += struct.pack('<I', 1)  # unknown
        info += struct.pack('<I', 1)  # unknown
        info += struct.pack('<I', 120)  # zone_id (arbitrary)
        info += b'\x00' * 64  # zone_name padding
        info += struct.pack('<I', 0)  # unknown

        self.send_app(addr, OP_SEND_CHAR_INFO, info)
        print(f"[world] Sent OP_SEND_CHAR_INFO for '{self.char_name}'")

    def send_zone_server_info(self, addr):
        """Send OP_ZONE_SERVER_INFO pointing to the zone server."""
        zone_host = "127.0.0.1"
        zone_port = self.zone_port

        # ZoneServerInfo_S: ip(128) + port(2)
        ip_field = zone_host.encode() + b'\x00' * (128 - len(zone_host))
        info = ip_field + struct.pack('<H', zone_port)

        self.send_app(addr, OP_ZONE_SERVER_INFO, info)
        print(f"[world] Sent OP_ZONE_SERVER_INFO → {zone_host}:{zone_port}")


# ── Zone Server ───────────────────────────────────────────────────────────

class ZoneServer:
    """Minimal zone server that puts the client into testzone."""

    # NewZone_S is 688 bytes. Fields we must set:
    #   char_name: [u8; 64] @ 0
    #   zone_short: [u8; 32] @ 64
    #   zone_long: [u8; 278] @ 96
    #   safe_y: f32 @ ... (we'll compute offset)
    #   safe_x: f32
    #   safe_z: f32
    #   zone_id: u16 @ 684
    #   zone_instance: u16 @ 686

    def __init__(self, port, char_name="Aiquestbot", zone_name="testzone"):
        self.port = port
        self.char_name = char_name
        self.zone_name = zone_name
        self.sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
        self.sock.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        self.sock.bind(('127.0.0.1', port))
        self.sock.settimeout(0.5)
        self.running = False
        self._sessions = {}
        self._got_zone_entry = False
        self._got_req_client_spawn = False
        self._got_req_new_zone = False

    def start(self):
        self.running = True
        print(f"[zone] Listening on 127.0.0.1:{self.port}")
        while self.running:
            try:
                data, addr = self.sock.recvfrom(8192)
            except socket.timeout:
                continue
            except OSError:
                break
            self.handle_packet(data, addr)

    def stop(self):
        self.running = False
        self.sock.close()

    def handle_packet(self, data, addr):
        if len(data) < 2:
            return
        opcode = data[1]
        payload = data[2:]

        if opcode == OP_SESSION_REQUEST:
            self.handle_session(data, addr)
            return

        if opcode == OP_PACKET and len(payload) >= 2:
            seq = struct.unpack('>H', payload[0:2])[0]
            app_data = payload[2:]
            if len(app_data) >= 2:
                app_opcode = struct.unpack('<H', app_data[0:2])[0]
                app_payload = app_data[2:]
                self.dispatch_app(app_opcode, app_payload, addr, seq)

    def handle_session(self, data, addr):
        if len(data) < 14:
            return
        connect_code = struct.unpack('>I', data[6:10])[0]
        max_size = struct.unpack('>I', data[10:14])[0]

        encode_key = random.randint(1, 0x7FFFFFFF)
        reply = struct.pack('>I', connect_code)
        reply += struct.pack('>I', encode_key)
        reply += bytes([0, 0, 0])
        reply += struct.pack('>I', max_size)

        raw = bytes([0x00, OP_SESSION_RESPONSE]) + reply
        self.sock.sendto(raw, addr)
        self._sessions[addr] = encode_key
        print(f"[zone] Session established with {addr}")

    def send_app(self, addr, opcode, payload, seq=0):
        app_data = struct.pack('<H', opcode) + payload
        inner = struct.pack('>H', seq) + app_data
        raw = bytes([0x00, OP_PACKET]) + inner
        self.sock.sendto(raw, addr)

    def send_ack(self, addr, seq):
        raw = bytes([0x00, OP_ACK]) + struct.pack('>H', seq)
        self.sock.sendto(raw, addr)

    def dispatch_app(self, opcode, payload, addr, seq):
        self.send_ack(addr, seq)
        print(f"[zone] Got opcode 0x{opcode:04x} ({len(payload)} bytes)")

        if opcode == OP_ZONE_ENTRY:
            self._got_zone_entry = True
            # Send player's own spawn (Spawn_S) back
            self.send_zone_entry_response(addr)
            # Send OP_NEW_ZONE
            self.send_new_zone(addr)
            # Send OP_WEATHER
            self.send_app(addr, OP_WEATHER, struct.pack('<I', 0))
            print("[zone] Sent OP_WEATHER")

        elif opcode == OP_REQ_CLIENT_SPAWN:
            self._got_req_client_spawn = True
            # Send some NPC spawns for visual testing
            self.send_npc_spawns(addr)
            # Send player profile
            self.send_player_profile(addr)

        elif opcode == OP_REQ_NEW_ZONE:
            self._got_req_new_zone = True
            # Send OP_SEND_EXP_ZONE_IN to trigger client ready
            self.send_app(addr, OP_SEND_EXP_ZONE_IN, b'\x00' * 4)
            print("[zone] Sent OP_SEND_EXP_ZONE_IN")

        elif opcode == OP_SEND_EXP_ZONE_IN:
            # Client echoed back — send OP_CLIENT_READY
            self.send_app(addr, OP_CLIENT_READY, b'\x00' * 4)
            print("[zone] Sent OP_CLIENT_READY — zone entry complete!")

    def build_spawn_s(self, spawn_id, name, race, class_id, level, x, y, z, heading=0, is_npc=True):
        """Build a Spawn_S struct (variable size, but we use a fixed allocation)."""
        # Spawn_S layout from Titanium (approx 384 bytes):
        # We'll build a minimal valid one
        name_bytes = name.encode()[:31]
        name_field = name_bytes + b'\x00' * (32 - len(name_bytes))

        buf = bytearray(384)
        # Struct fields (approximate offsets for Titanium Spawn_S)
        struct.pack_into('<I', buf, 0, spawn_id)      # spawnId
        struct.pack_into('<H', buf, 4, race % 0x100)  # race (low byte)
        buf[6] = class_id % 256                        # class
        buf[7] = level                                  # level
        buf[8:40] = name_field                         # name

        # Position bitfields — encode (x, y, z) as fixed-point
        # EQ uses bitfield encoding: 18 bits for each axis, with a base offset
        x_encoded = int((x + 0.5) * 2.0) & 0x3FFFF
        y_encoded = int((y + 0.5) * 2.0) & 0x3FFFF
        z_encoded = int((z + 0.5) * 2.0) & 0x3FFFF

        # bitfield_pos1 contains Y (north/server_x) in upper bits
        # bitfield_pos2 contains X (east/server_y) in upper bits
        # bitfield_pos3 contains Z (height) in upper bits
        bitfield_pos1 = (y_encoded & 0x3FFFF) << 3
        bitfield_pos2 = (x_encoded & 0x3FFFF) << 3
        bitfield_pos3 = (z_encoded & 0x3FFFF) << 3
        bitfield_pos4 = 0  # heading

        struct.pack_into('<I', buf, 40, bitfield_pos1)
        struct.pack_into('<I', buf, 44, bitfield_pos2)
        struct.pack_into('<I', buf, 48, bitfield_pos3)
        struct.pack_into('<I', buf, 52, bitfield_pos4)

        # Some additional fields
        if is_npc:
            buf[56] = 0x04  # NPC type flag
        else:
            buf[56] = 0x00  # Player

        return bytes(buf)

    def send_zone_entry_response(self, addr):
        """Send player's own Spawn_S in response to OP_ZONE_ENTRY."""
        name = self.char_name
        spawn = self.build_spawn_s(
            spawn_id=1,
            name=name,
            race=1,      # Human
            class_id=1,  # Warrior
            level=1,
            x=0.0, y=0.0, z=5.0,  # slightly above ground
            is_npc=False
        )
        # Server echoes back with possible 2-byte prefix
        self.send_app(addr, OP_ZONE_ENTRY, b'\x00\x00' + spawn)
        print(f"[zone] Sent player spawn: {name} id=1")

    def build_new_zone_s(self, char_name, zone_name, zone_id=9999):
        """Build a NewZone_S struct (688 bytes)."""
        buf = bytearray(688)

        # char_name @ 0
        name_bytes = char_name.encode()[:63]
        buf[0:len(name_bytes)] = name_bytes

        # zone_short @ 64
        zone_bytes = zone_name.encode()[:31]
        buf[64:64+len(zone_bytes)] = zone_bytes

        # zone_long @ 96 (278 bytes)
        long_name = f"The {zone_name.title()} Zone"
        long_bytes = long_name.encode()[:277]
        buf[96:96+len(long_bytes)] = long_bytes

        # ztype @ 374 (1 byte)
        buf[374] = 0x64  # indoor/outdoor type

        # gravity @ 392
        struct.pack_into('<f', buf, 392, -0.85)

        # sky @ 413
        buf[413] = 1  # normal sky

        # zone_exp_mult @ 426
        struct.pack_into('<f', buf, 426, 1.0)

        # safe_y @ 430
        struct.pack_into('<f', buf, 430, 0.0)
        # safe_x @ 434
        struct.pack_into('<f', buf, 434, 0.0)
        # safe_z @ 438
        struct.pack_into('<f', buf, 438, 5.0)

        # max_z @ 442
        struct.pack_into('<f', buf, 442, 500.0)
        # underworld @ 446
        struct.pack_into('<f', buf, 446, -500.0)

        # minclip @ 450
        struct.pack_into('<f', buf, 450, 0.5)
        # maxclip @ 454
        struct.pack_into('<f', buf, 454, 600.0)

        # zone_short2 @ 616 (68 bytes)
        buf[616:616+len(zone_bytes)] = zone_bytes

        # zone_id @ 684
        struct.pack_into('<H', buf, 684, zone_id)
        # zone_instance @ 686
        struct.pack_into('<H', buf, 686, 0)

        return bytes(buf)

    def send_new_zone(self, addr):
        """Send OP_NEW_ZONE with the zone info."""
        new_zone = self.build_new_zone_s(self.char_name, self.zone_name)
        self.send_app(addr, OP_NEW_ZONE, new_zone)
        print(f"[zone] Sent OP_NEW_ZONE: {self.zone_name}")

    def send_npc_spawns(self, addr):
        """Send some NPC spawns for visual testing."""
        npcs = [
            (100, "a_test_humanoid", 1, 1, 1,  10.0,  0.0, 5.0),
            (101, "a_test_elf",      2, 1, 1,  20.0,  0.0, 5.0),
            (102, "a_test_dwarf",    3, 1, 1,  30.0,  0.0, 5.0),
            (103, "a_test_skeleton", 4, 1, 1,  40.0,  0.0, 5.0),
            (104, "a_test_zombie",   5, 1, 1,  50.0,  0.0, 5.0),
            (105, "a_test_rat",      6, 1, 1,  60.0,  0.0, 3.0),
            (106, "a_test_wolf",     7, 1, 1,  70.0,  0.0, 4.0),
            (107, "a_test_bear",     8, 1, 1,  80.0,  0.0, 5.0),
            (108, "a_test_bat",      9, 1, 1,  90.0,  0.0, 8.0),
            (109, "a_test_snake",   10, 1, 1, 100.0,  0.0, 3.0),
            (110, "a_test_frog",    11, 1, 1, 110.0,  0.0, 2.0),
            (111, "a_test_wasp",    12, 1, 1, 120.0,  0.0, 10.0),
            (112, "a_test_bird",    13, 1, 1, 130.0,  0.0, 12.0),
            (113, "a_test_worm",    14, 1, 1, 140.0,  0.0, 1.0),
            (114, "a_test_fish",    15, 1, 1, 150.0,  0.0, -2.0),
        ]

        # Build zone spawns packet: multiple Spawn_S concatenated
        spawns_data = b''
        for spawn_id, name, race, class_id, level, x, y, z in npcs:
            spawn = self.build_spawn_s(spawn_id, name, race, class_id, level, x, y, z)
            spawns_data += spawn

        self.send_app(addr, OP_ZONE_SPAWNS, spawns_data)
        print(f"[zone] Sent {len(npcs)} NPC spawns")

    def send_player_profile(self, addr):
        """Send a minimal OP_PLAYER_PROFILE."""
        # PlayerProfile_S is large (4444+ bytes), but we only need a few fields
        buf = bytearray(4444)
        # class @ 12
        struct.pack_into('<I', buf, 12, 1)  # Warrior
        # level @ 20
        buf[20] = 1
        # stats @ 2236..2260 (7 stats, all 25)
        for i in range(7):
            struct.pack_into('<I', buf, 2236 + i * 4, 25)
        # coin @ 4428..4440
        struct.pack_into('<I', buf, 4428, 1000)  # platinum

        self.send_app(addr, OP_PLAYER_PROFILE, bytes(buf))
        print("[zone] Sent OP_PLAYER_PROFILE")


# ── Main ──────────────────────────────────────────────────────────────────

def main():
    parser = argparse.ArgumentParser(description="Mock EQ server for client testing")
    parser.add_argument('--login-port', type=int, default=5998, help='Login server UDP port')
    parser.add_argument('--world-port', type=int, default=9000, help='World server UDP port')
    parser.add_argument('--zone-port', type=int, default=9001, help='Zone server UDP port')
    parser.add_argument('--char-name', default='Aiquestbot', help='Character name')
    parser.add_argument('--zone', default='testzone', help='Zone name to enter')
    args = parser.parse_args()

    print(f"=== Mock EQ Server ===")
    print(f"Login:  127.0.0.1:{args.login_port}")
    print(f"World:  127.0.0.1:{args.world_port}")
    print(f"Zone:   127.0.0.1:{args.zone_port}")
    print(f"Zone:   {args.zone}")
    print(f"Char:   {args.char_name}")
    print()

    login = LoginServer(args.login_port, '127.0.0.1', args.world_port, args.zone_port)
    world = WorldServer(args.world_port, args.zone_port, args.char_name)
    zone = ZoneServer(args.zone_port, args.char_name, args.zone)

    login_thread = threading.Thread(target=login.start, daemon=True)
    world_thread = threading.Thread(target=world.start, daemon=True)
    zone_thread = threading.Thread(target=zone.start, daemon=True)

    login_thread.start()
    world_thread.start()
    zone_thread.start()

    print("All servers running. Connect the client to 127.0.0.1:5998")
    print("Press Ctrl+C to stop\n")

    try:
        while True:
            time.sleep(1)
    except KeyboardInterrupt:
        print("\nShutting down...")
        login.stop()
        world.stop()
        zone.stop()
        sys.exit(0)

if __name__ == '__main__':
    main()
