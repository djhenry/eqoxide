//! EQ transport layer: UDP stream, session management, CRC, compression, fragmentation.
//!
//! Ported from the Python reference at eq_client/connection/stream.py.

use std::collections::HashMap;
use std::io::Cursor;
use std::net::SocketAddr;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use tokio::net::UdpSocket;
use tokio::sync::mpsc;

use super::protocol::*;

// ── CRC32 table ────────────────────────────────────────────────────────────

const CRC32_TABLE: [u32; 256] = [
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
];

/// EQ CRC32 keyed by session encode_key — matches EQ::Crc32(data, size, key).
fn eq_crc32(data: &[u8], key: u32) -> u32 {
    let key = key & 0xFFFFFFFF;
    let mut crc: u32 = 0xFFFFFFFF;
    for i in 0..4 {
        let b = ((key >> (i * 8)) & 0xFF) as u8;
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ b as u32) & 0xFF) as usize];
    }
    for b in data {
        crc = ((crc >> 8) & 0x00FFFFFF) ^ CRC32_TABLE[((crc ^ *b as u32) & 0xFF) as usize];
    }
    (!crc) & 0xFFFFFFFF
}

/// XOR-encode/decode with 4-byte rolling key.
fn decode_xor(data: &[u8], key: u32) -> Vec<u8> {
    let key_bytes = key.to_be_bytes();
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ key_bytes[i % 4])
        .collect()
}

/// EQ compression: 0x5a + zlib if beneficial and data > 30 bytes, else 0xa5 + raw.
fn eq_compress(data: &[u8]) -> Vec<u8> {
    if data.len() > 30 {
        let compressed = miniz_oxide::deflate::compress_to_vec_zlib(data, 1);
        if compressed.len() < data.len() {
            let mut result = vec![0x5a];
            result.extend_from_slice(&compressed);
            return result;
        }
    }
    let mut result = vec![0xa5];
    result.extend_from_slice(data);
    result
}

/// EQ decompression: 0x5a = zlib, 0xa5 = raw, else passthrough.
fn eq_decompress(data: &[u8]) -> Option<Vec<u8>> {
    if data.is_empty() {
        return Some(vec![]);
    }
    match data[0] {
        0x5a => {
            miniz_oxide::inflate::decompress_to_vec_zlib(&data[1..]).ok()
        }
        0xa5 => Some(data[1..].to_vec()),
        _ => Some(data.to_vec()),
    }
}

// ── Session info ───────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct SessionInfo {
    pub connect_code: u32,
    pub encode_key: u32,
    pub crc_bytes: u8,
    pub encode_pass1: u8,
    pub encode_pass2: u8,
    pub max_packet_size: u16,
    pub connected: bool,
}

impl Default for SessionInfo {
    fn default() -> Self {
        SessionInfo {
            connect_code: 0,
            encode_key: 0,
            crc_bytes: 0,
            encode_pass1: 0,
            encode_pass2: 0,
            max_packet_size: 512,
            connected: false,
        }
    }
}

// ── Fragment buffer ────────────────────────────────────────────────────────

struct FragmentBuffer {
    buf: Vec<u8>,
    total: usize,
}

impl FragmentBuffer {
    fn new() -> Self {
        FragmentBuffer { buf: Vec::new(), total: 0 }
    }

    fn in_progress(&self) -> bool {
        self.total > 0
    }

    /// Feed one fragment. Returns complete reassembled data if done.
    /// For the first fragment, `data` starts with 4-byte big-endian total_size.
    fn add(&mut self, data: &[u8], _is_first: bool) -> Option<Vec<u8>> {
        if !self.in_progress() {
            if data.len() < 4 {
                return None;
            }
            self.total = Cursor::new(&data[..4]).read_u32::<BigEndian>().unwrap() as usize;
            self.buf.extend_from_slice(&data[4..]);
        } else {
            self.buf.extend_from_slice(data);
        }
        if self.buf.len() >= self.total {
            let result = self.buf[..self.total].to_vec();
            self.buf.clear();
            self.total = 0;
            Some(result)
        } else {
            None
        }
    }
}

// ── App packet type ────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
pub struct AppPacket {
    pub opcode: u16,
    pub payload: Vec<u8>,
}

// ── EQ Stream ──────────────────────────────────────────────────────────────

pub struct EqStream {
    session: SessionInfo,
    socket: UdpSocket,
    #[allow(dead_code)]
    peer: SocketAddr,
    send_seq: u16,
    next_recv_seq: u16,
    recv_buf: HashMap<u16, (Vec<u8>, bool)>, // seq → (data, is_fragment)
    frags: FragmentBuffer,
    app_tx: mpsc::UnboundedSender<AppPacket>,
}

impl EqStream {
    pub async fn connect(
        host: &str,
        port: u16,
        app_tx: mpsc::UnboundedSender<AppPacket>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let peer: SocketAddr = format!("{}:{}", host, port).parse()?;
        let socket = UdpSocket::bind("0.0.0.0:0").await?;
        socket.connect(peer).await?;

        let mut stream = EqStream {
            session: SessionInfo::default(),
            socket,
            peer,
            send_seq: 0,
            next_recv_seq: 0,
            recv_buf: HashMap::new(),
            frags: FragmentBuffer::new(),
            app_tx,
        };

        stream.send_session_request();

        // Wait for SESSION_RESPONSE. Re-send SESSION_REQUEST every 1s in case of UDP loss.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        let mut last_send = std::time::Instant::now();
        let mut recv_buf = vec![0u8; 4096];
        while !stream.session.connected {
            if std::time::Instant::now() > deadline {
                return Err("Session handshake timeout: no SESSION_RESPONSE from server".into());
            }
            if last_send.elapsed() >= std::time::Duration::from_secs(1) {
                stream.send_session_request();
                last_send = std::time::Instant::now();
            }
            match tokio::time::timeout(
                tokio::time::Duration::from_millis(100),
                stream.socket.recv(&mut recv_buf),
            ).await {
                Ok(Ok(n)) => stream.on_raw_recv(&recv_buf[..n]),
                Ok(Err(e)) => return Err(e.into()),
                Err(_) => {} // recv timeout, keep waiting
            }
        }

        Ok(stream)
    }

    /// Send a session request (must be called after connect, before any other packets).
    fn send_session_request(&mut self) {
        let connect_code: u32 = rand::random::<u32>() & 0x7FFFFFFF;
        self.session.connect_code = connect_code;
        let mut payload = Vec::new();
        payload.write_u32::<BigEndian>(2).unwrap(); // protocol version
        payload.write_u32::<BigEndian>(connect_code).unwrap();
        payload.write_u32::<BigEndian>(self.session.max_packet_size as u32).unwrap();
        self.send_raw(OP_SESSION_REQUEST, &payload);
    }

    /// Send an application-level EQ packet (2-byte LE opcode + payload).
    pub fn send_app_packet(&mut self, opcode: u16, payload: &[u8]) {
        let mut app_data = Vec::with_capacity(2 + payload.len());
        app_data.write_u16::<byteorder::LittleEndian>(opcode).unwrap();
        app_data.extend_from_slice(payload);
        self.send_reliable(&app_data);
    }

    /// Poll for incoming data. Non-blocking. Returns false if the socket is closed.
    pub fn poll_recv(&mut self) -> bool {
        let mut buf = vec![0u8; 4096];
        match self.socket.try_recv(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                self.on_raw_recv(&buf);
                true
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => true,
            Err(_) => false,
        }
    }

    /// Send a keepalive response.
    pub fn send_keepalive(&mut self) {
        self.send_raw(OP_KEEPALIVE, &[]);
    }

    /// Send a session-layer disconnect (`OP_SessionDisconnect`, 0x05). Tells the EQStream peer
    /// we are closing this session. Payload is the negotiated `connect_code` as a big-endian u32;
    /// `append_crc` (called inside `send_raw`) appends the CRC. Sent as part of clean shutdown.
    pub fn send_session_disconnect(&mut self) {
        let mut payload = Vec::with_capacity(4);
        payload.write_u32::<BigEndian>(self.session.connect_code).unwrap();
        self.send_raw(OP_SESSION_DISC, &payload);
    }

    // ── Internal send helpers ─────────────────────────────────────────────────

    fn send_raw(&mut self, opcode: u8, payload: &[u8]) {
        let mut raw = vec![0x00, opcode];
        raw.extend_from_slice(payload);
        raw = self.append_crc(raw);
        let _ = self.socket.try_send(&raw);
    }

    fn append_crc(&self, data: Vec<u8>) -> Vec<u8> {
        match self.session.crc_bytes {
            4 => {
                let crc = eq_crc32(&data, self.session.encode_key);
                let mut data = data;
                data.write_u32::<BigEndian>(crc).unwrap();
                data
            }
            2 => {
                let crc = eq_crc32(&data, self.session.encode_key) & 0xFFFF;
                let mut data = data;
                data.write_u16::<BigEndian>(crc as u16).unwrap();
                data
            }
            1 => {
                let crc = eq_crc32(&data, self.session.encode_key) & 0xFF;
                let mut data = data;
                data.push(crc as u8);
                data
            }
            _ => data,
        }
    }

    fn send_ack(&mut self, seq: u16) {
        let seq_bytes = seq.to_be_bytes();
        self.send_raw(OP_ACK, &self.encode(&seq_bytes.to_vec()));
    }

    fn send_reliable(&mut self, app_data: &[u8]) {
        let max_inner = (self.session.max_packet_size as usize) - 5; // 2 proto + 1 compress + 2 crc
        if app_data.len() + 2 <= max_inner {
            let seq = self.next_send_seq();
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(app_data);
            self.send_raw(OP_PACKET, &self.encode(&inner));
        } else {
            // Fragment
            let seq = self.next_send_seq();
            let total_size = app_data.len() as u32;
            let first_max = max_inner - 2 - 4; // seq + total_size overhead
            let mut inner = seq.to_be_bytes().to_vec();
            inner.extend_from_slice(&total_size.to_be_bytes());
            inner.extend_from_slice(&app_data[..first_max]);
            self.send_raw(OP_FRAGMENT, &self.encode(&inner));

            let mut offset = first_max;
            while offset < app_data.len() {
                let seq = self.next_send_seq();
                let end = (offset + max_inner - 2).min(app_data.len());
                let mut inner = seq.to_be_bytes().to_vec();
                inner.extend_from_slice(&app_data[offset..end]);
                self.send_raw(OP_FRAGMENT, &self.encode(&inner));
                offset = end;
            }
        }
    }

    fn next_send_seq(&mut self) -> u16 {
        let seq = self.send_seq;
        self.send_seq = self.send_seq.wrapping_add(1);
        seq
    }

    // ── Encoding/decoding ─────────────────────────────────────────────────────

    fn encode(&self, data: &[u8]) -> Vec<u8> {
        let mut result = data.to_vec();
        if self.session.encode_pass1 == ENCODE_COMPRESSION {
            result = eq_compress(&result);
        } else if self.session.encode_pass1 == ENCODE_XOR {
            result = decode_xor(&result, self.session.encode_key);
        }
        if self.session.encode_pass2 == ENCODE_COMPRESSION {
            result = eq_compress(&result);
        } else if self.session.encode_pass2 == ENCODE_XOR {
            result = decode_xor(&result, self.session.encode_key);
        }
        result
    }

    fn decode(&self, data: &[u8]) -> Option<Vec<u8>> {
        let mut result = data.to_vec();
        if self.session.encode_pass2 == ENCODE_COMPRESSION {
            result = eq_decompress(&result)?;
        } else if self.session.encode_pass2 == ENCODE_XOR {
            result = decode_xor(&result, self.session.encode_key);
        }
        if self.session.encode_pass1 == ENCODE_COMPRESSION {
            result = eq_decompress(&result)?;
        } else if self.session.encode_pass1 == ENCODE_XOR {
            result = decode_xor(&result, self.session.encode_key);
        }
        Some(result)
    }

    // ── Receive dispatch ──────────────────────────────────────────────────────

    fn on_raw_recv(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let opcode = data[1];
        let mut payload = data[2..].to_vec();

        // Strip outer CRC
        if self.session.crc_bytes == 4 && payload.len() >= 4 {
            payload = payload[..payload.len() - 4].to_vec();
        } else if self.session.crc_bytes == 2 && payload.len() >= 2 {
            payload = payload[..payload.len() - 2].to_vec();
        } else if self.session.crc_bytes == 1 && payload.len() >= 1 {
            payload = payload[..payload.len() - 1].to_vec();
        }

        self.dispatch_transport(opcode, &payload);
    }

    fn dispatch_transport(&mut self, opcode: u8, payload: &[u8]) {
        match opcode {
            OP_SESSION_RESPONSE => self.handle_session_response(payload),
            OP_KEEPALIVE => { self.send_raw(OP_KEEPALIVE, &[]); }
            OP_STAT_REQUEST => { self.send_raw(OP_STAT_RESPONSE, payload); }
            OP_COMBINED => self.handle_transport_combined(payload),
            OP_PACKET => self.handle_packet(payload),
            OP_FRAGMENT | OP_FRAGMENT_CONT | OP_FRAGMENT_CONT2 | OP_FRAGMENT_CONT3 => {
                self.handle_fragment(payload);
            }
            OP_APP_COMBINED => self.handle_combined(payload),
            OP_ACK | OP_OUT_OF_ORDER => {} // no retransmit tracking
            _ => {}
        }
    }

    fn handle_session_response(&mut self, payload: &[u8]) {
        if payload.len() < 15 {
            return;
        }
        // ReliableStreamConnectReply layout (all BE):
        //   connect_code(4) encode_key(4) crc_bytes(1) encode_pass1(1) encode_pass2(1) max_size(4)
        self.session.connect_code = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]);
        self.session.encode_key   = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        self.session.crc_bytes    = payload[8];
        self.session.encode_pass1 = payload[9];
        self.session.encode_pass2 = payload[10];
        if payload.len() >= 15 {
            let max = u32::from_be_bytes([payload[11], payload[12], payload[13], payload[14]]);
            if max > 0 {
                self.session.max_packet_size = max.min(0xFFFF) as u16;
            }
        }
        self.session.connected = true;
    }

    fn handle_ordered(&mut self, payload: &[u8], is_fragment: bool) {
        if payload.len() < 2 {
            return;
        }
        let decoded = match self.decode(payload) {
            Some(d) => d,
            None => return,
        };
        if decoded.len() < 2 {
            return;
        }
        let seq = Cursor::new(&decoded[..2]).read_u16::<BigEndian>().unwrap();
        let data = decoded[2..].to_vec();

        if seq == self.next_recv_seq {
            self.next_recv_seq = self.next_recv_seq.wrapping_add(1);
            self.deliver_seq(seq, data, is_fragment);
            // Drain buffered continuations
            while let Some((ndata, nfrag)) = self.recv_buf.remove(&self.next_recv_seq) {
                let nseq = self.next_recv_seq;
                self.next_recv_seq = self.next_recv_seq.wrapping_add(1);
                self.deliver_seq(nseq, ndata, nfrag);
            }
        } else if seq > self.next_recv_seq || (seq < 0x1000 && self.next_recv_seq > 0xF000) {
            self.recv_buf.insert(seq, (data, is_fragment));
            self.send_raw(OP_OUT_OF_ORDER, &seq.to_be_bytes());
        }
    }

    fn deliver_seq(&mut self, seq: u16, data: Vec<u8>, is_fragment: bool) {
        self.send_ack(seq);
        if is_fragment {
            if let Some(complete) = self.frags.add(&data, !self.frags.in_progress()) {
                self.dispatch_app(&complete);
            }
        } else {
            self.dispatch_app(&data);
        }
    }

    fn handle_packet(&mut self, payload: &[u8]) {
        self.handle_ordered(payload, false);
    }

    fn handle_fragment(&mut self, payload: &[u8]) {
        self.handle_ordered(payload, true);
    }

    fn handle_transport_combined(&mut self, payload: &[u8]) {
        let payload = match self.decode(payload) {
            Some(d) => d,
            None => return,
        };
        let mut offset = 0;
        while offset < payload.len() {
            let sub_len = payload[offset] as usize;
            offset += 1;
            if offset + sub_len > payload.len() {
                break;
            }
            let sub = &payload[offset..offset + sub_len];
            if sub.len() >= 2 {
                self.dispatch_transport(sub[1], &sub[2..]);
            }
            offset += sub_len;
        }
    }

    fn handle_combined(&mut self, payload: &[u8]) {
        let mut offset = 0;
        while offset < payload.len() {
            let mut sub_len = payload[offset] as usize;
            offset += 1;
            if sub_len == 0xFF && offset + 2 <= payload.len() {
                sub_len = Cursor::new(&payload[offset..offset + 2]).read_u16::<BigEndian>().unwrap() as usize;
                offset += 2;
            }
            if offset + sub_len > payload.len() {
                break;
            }
            self.dispatch_app(&payload[offset..offset + sub_len]);
            offset += sub_len;
        }
    }

    fn dispatch_app(&mut self, data: &[u8]) {
        if data.len() < 2 {
            return;
        }
        let opcode = Cursor::new(&data[..2]).read_u16::<byteorder::LittleEndian>().unwrap();
        let payload = data[2..].to_vec();
        let _ = self.app_tx.send(AppPacket { opcode, payload });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crc32_zero_key() {
        let data = b"hello world";
        let crc = eq_crc32(data, 0);
        // Just verify it doesn't panic and produces a deterministic value
        let crc2 = eq_crc32(data, 0);
        assert_eq!(crc, crc2);
    }

    #[test]
    fn test_crc32_keyed() {
        let data = b"test";
        let crc1 = eq_crc32(data, 0x12345678);
        let crc2 = eq_crc32(data, 0x12345678);
        let crc3 = eq_crc32(data, 0x87654321);
        assert_eq!(crc1, crc2);
        assert_ne!(crc1, crc3);
    }

    #[test]
    fn test_xor_roundtrip() {
        let data = vec![0x01, 0x02, 0x03, 0x04, 0x05];
        let key: u32 = 0xDEADBEEF;
        let encoded = decode_xor(&data, key);
        let decoded = decode_xor(&encoded, key);
        assert_eq!(data, decoded);
    }

    #[test]
    fn test_compress_roundtrip() {
        let data = b"hello world this is a test of the compression system";
        let compressed = eq_compress(data);
        let decompressed = eq_decompress(&compressed).unwrap();
        assert_eq!(data.to_vec(), decompressed);
    }

    #[test]
    fn test_compress_small_data() {
        let data = b"short";
        let compressed = eq_compress(data);
        assert_eq!(compressed[0], 0xa5); // raw prefix for small data
        assert_eq!(&compressed[1..], data);
    }

    #[test]
    fn test_fragment_buffer_single() {
        let mut fb = FragmentBuffer::new();
        let data = vec![0u8; 100];
        let mut prefixed = (data.len() as u32).to_be_bytes().to_vec();
        prefixed.extend_from_slice(&data);
        let result = fb.add(&prefixed, true);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), data);
    }

    #[test]
    fn test_fragment_buffer_multi() {
        let mut fb = FragmentBuffer::new();
        let total = vec![0xABu8; 200];
        let first_chunk = &total[..100];
        let second_chunk = &total[100..];

        let mut prefixed = (total.len() as u32).to_be_bytes().to_vec();
        prefixed.extend_from_slice(first_chunk);
        let result = fb.add(&prefixed, true);
        assert!(result.is_none());

        let result = fb.add(second_chunk, false);
        assert!(result.is_some());
        assert_eq!(result.unwrap(), total);
    }
}
