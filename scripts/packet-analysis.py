#!/usr/bin/env python3
"""Summarize the eqoxide packet-telemetry ring (#525).

Pulls GET /v1/observe/packets from a running client and prints a readable report:
an opcode histogram (count / bytes / rate), per-direction totals, and reliable
SEQUENCE-GAP detection — the signal that diagnoses spawn-tail drops (#463).

The heavy lifting (histogram + gap detection) is done server-side by the endpoint's
`?summary=1`; this script just fetches and formats it, so the analysis logic lives in
one place (src/eq_net/packet_telemetry.rs) and can't drift. `--raw` dumps the record
list instead.

Capture is DEFAULT-OFF. Enable it either at client startup with EQOXIDE_PKTLOG=1, or
per-request here with `--enable` (then drive the scenario, then re-run to read).

Examples:
    # turn capture on, clear the buffer, then (after doing something) read a summary
    scripts/packet-analysis.py --port 8765 --enable --clear
    scripts/packet-analysis.py --port 8765                    # histogram + seq gaps
    scripts/packet-analysis.py --port 8765 --dir in --op 0x5089   # per-spawn stream
    scripts/packet-analysis.py --port 8765 --raw --limit 50   # last 50 raw records
"""
import argparse
import json
import sys
import urllib.parse
import urllib.request


def fetch(port, params):
    qs = urllib.parse.urlencode({k: v for k, v in params.items() if v is not None})
    url = f"http://127.0.0.1:{port}/v1/observe/packets"
    if qs:
        url += "?" + qs
    with urllib.request.urlopen(url, timeout=10) as resp:
        return json.load(resp)


def print_summary(data):
    enabled = data.get("enabled")
    s = data.get("summary", {})
    print(f"capture enabled: {enabled}")
    print(f"total={s.get('total', 0)}  in={s.get('in_count', 0)}  "
          f"out={s.get('out_count', 0)}  window={s.get('window_ms', 0)}ms")
    print()
    print("OPCODE HISTOGRAM (by count):")
    print(f"  {'dir':<4} {'opcode':<8} {'name':<32} {'count':>7} {'bytes':>9} {'rate/s':>9}")
    for st in s.get("histogram", []):
        print(f"  {st['dir']:<4} {st['op_hex']:<8} {st['op_name']:<32} "
              f"{st['count']:>7} {st['bytes']:>9} {st['rate_per_sec']:>9.2f}")
    print()
    gaps = s.get("seq_gaps", [])
    if not gaps:
        print("RELIABLE SEQ GAPS: none detected")
    else:
        print(f"RELIABLE SEQ GAPS: {len(gaps)} detected")
        for g in gaps:
            print(f"  {g['dir']:<3} after n={g['after_n']}: "
                  f"seq {g['prev_seq']} -> {g['next_seq']} "
                  f"({g['missing']} missing)")
    note = s.get("seq_gap_note")
    if note:
        print()
        print("NOTE:", note)


def print_raw(data):
    print(f"capture enabled: {data.get('enabled')}  count={data.get('count', 0)}")
    for r in data.get("packets", []):
        seq = r.get("rel_seq")
        seq_s = f"seq={seq}" if seq is not None else "seq=-"
        rel = "R" if r.get("reliable") else "u"
        summ = f"  {r['summary']}" if r.get("summary") else ""
        print(f"  n={r['n']:<6} t={r['t_ms']:>7}ms {r['dir']:<3} {rel} "
              f"{r['op_hex']} {r['op_name']:<28} {r['size']:>5}B {seq_s}{summ}")


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--port", type=int, default=8765, help="client API port (default 8765)")
    ap.add_argument("--since", type=int, help="only records with n >= SINCE")
    ap.add_argument("--limit", type=int, help="cap the number of records")
    ap.add_argument("--dir", choices=["in", "out"], help="filter by direction")
    ap.add_argument("--op", help="filter by opcode (hex 0x... or decimal)")
    ap.add_argument("--enable", action="store_true", help="turn capture ON before reading")
    ap.add_argument("--disable", action="store_true", help="turn capture OFF before reading")
    ap.add_argument("--clear", action="store_true", help="clear the buffer before reading")
    ap.add_argument("--raw", action="store_true", help="dump raw records instead of a summary")
    args = ap.parse_args()

    params = {"since": args.since, "limit": args.limit, "dir": args.dir, "op": args.op}
    if args.enable:
        params["enable"] = "1"
    elif args.disable:
        params["enable"] = "0"
    if args.clear:
        params["clear"] = "1"
    if not args.raw:
        params["summary"] = "1"

    try:
        data = fetch(args.port, params)
    except Exception as e:  # noqa: BLE001 - a CLI helper, surface any fetch error plainly
        print(f"error: could not reach client on port {args.port}: {e}", file=sys.stderr)
        return 1

    if args.raw:
        print_raw(data)
    else:
        print_summary(data)
    return 0


if __name__ == "__main__":
    sys.exit(main())
