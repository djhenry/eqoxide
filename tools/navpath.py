#!/usr/bin/env python3
"""
navpath.py — A* pathfinding navigator for eq_client_lite.

Computes a walkable path using zone S3D geometry and posts waypoints to the
client's /goto HTTP endpoint one at a time, polling for arrival between each.

Usage:
    python3 tools/navpath.py <target>
    python3 tools/navpath.py <server_x> <server_y>
    python3 tools/navpath.py --entity "Guard Phaeton"

Target forms:
    "EntityName"         look up entity position via /entities
    X Y                  raw EQ server coords (x=north, y=east)
    --map MAP_X MAP_Y    EQ map file coords (same convention as map .txt files)

Options:
    --port PORT          HTTP port (default: 8765)
    --cell-size N        navgrid cell size in EQ units (default: 4.0)
    --z-band N           half-height of passable triangle filter (default: 200)
    --arrival N          arrival radius in EQ units (default: 15.0)
    --assets DIR         path to EQ assets dir (default: ~/eq_assets/EQ_Files)
    --dry-run            print waypoints without walking

Coordinate conventions:
    EQ server:  x=north/south (+x = north),  y=east/west (+y = east), z=height
    Scene space [east, north, height] = [server_y, server_x, server_z]
    S3D navgrid uses (server_x, server_y) as the (x, y) plane.
"""

import argparse
import math
import sys
import time
import os

import requests

EQ_NAV_PKG = os.path.expanduser("~/git/eq-client-ref")
if EQ_NAV_PKG not in sys.path:
    sys.path.insert(0, EQ_NAV_PKG)

from eq_client.nav.s3d import load_triangles
from eq_client.nav.navgrid import build_navgrid
from eq_client.nav.pathfinder import find_path


# ── helpers ──────────────────────────────────────────────────────────────────

def get_player_pos(base_url: str) -> tuple[float, float, float]:
    """Return current player position as (server_x, server_y, server_z)."""
    cam = requests.get(f"{base_url}/camera", timeout=3).json()
    focus = cam["focus"]  # [east, north, height] = [server_y, server_x, server_z]
    east, north, height = focus[0], focus[1], focus[2]
    return (north, east, height)  # → (server_x, server_y, server_z)


def get_entity_pos(base_url: str, name: str) -> tuple[float, float, float] | None:
    """Look up entity by (fuzzy) name. Returns (server_x, server_y, server_z)."""
    entities = requests.get(f"{base_url}/entities", timeout=3).json()
    name_lower = name.lower()
    # Exact match first, then fuzzy
    for key, coords in entities.items():
        if key.lower().rstrip("0123456789").rstrip() == name_lower:
            x, y, z = coords
            return (x, y, z)
    for key, coords in entities.items():
        if name_lower in key.lower():
            print(f"  fuzzy match: {key!r}", file=sys.stderr)
            x, y, z = coords
            return (x, y, z)
    return None


def get_zone_name(base_url: str) -> str:
    """Get current zone name from the client log or entities heuristic."""
    try:
        log = open("/tmp/eq_client.log").read()
        for line in reversed(log.splitlines()):
            if "zone:" in line and "sent ReqClientSpawn" in line:
                return line.split("zone:")[1].split("—")[0].strip()
    except Exception:
        pass
    return ""


def get_zone_point(base_url: str, zone_id: int) -> tuple[float, float, float] | None:
    """Return (server_x, server_y, server_z) of the zone exit for the given zone_id."""
    try:
        points = requests.get(f"{base_url}/zone_points", timeout=3).json()
        for p in points:
            if p["zone_id"] == zone_id:
                return (p["server_x"], p["server_y"], p["server_z"])
    except Exception as e:
        print(f"  WARNING: /zone_points fetch failed: {e}", file=sys.stderr)
    return None


def dist2d(a: tuple[float, float], b: tuple[float, float]) -> float:
    return math.sqrt((a[0] - b[0]) ** 2 + (a[1] - b[1]) ** 2)


def simplify_path(
    waypoints: list[tuple[float, float]],
    tolerance: float = 3.0,
) -> list[tuple[float, float]]:
    """Ramer-Douglas-Peucker path simplification."""
    if len(waypoints) <= 2:
        return waypoints

    def point_to_line_dist(p, a, b):
        ax, ay = a
        bx, by = b
        px, py = p
        dx, dy = bx - ax, by - ay
        if dx == 0 and dy == 0:
            return math.sqrt((px - ax) ** 2 + (py - ay) ** 2)
        t = max(0.0, min(1.0, ((px - ax) * dx + (py - ay) * dy) / (dx * dx + dy * dy)))
        nx, ny = ax + t * dx, ay + t * dy
        return math.sqrt((px - nx) ** 2 + (py - ny) ** 2)

    def rdp(pts, eps):
        if len(pts) < 3:
            return pts
        max_d, idx = 0.0, 0
        for i in range(1, len(pts) - 1):
            d = point_to_line_dist(pts[i], pts[0], pts[-1])
            if d > max_d:
                max_d, idx = d, i
        if max_d > eps:
            left  = rdp(pts[:idx + 1], eps)
            right = rdp(pts[idx:], eps)
            return left[:-1] + right
        return [pts[0], pts[-1]]

    return rdp(waypoints, tolerance)


def post_goto(base_url: str, server_x: float, server_y: float, server_z: float) -> bool:
    resp = requests.post(
        f"{base_url}/goto",
        json={"x": server_x, "y": server_y, "z": server_z},
        timeout=3,
    )
    return resp.status_code == 200


def wait_for_arrival(
    base_url: str,
    target_x: float,
    target_y: float,
    arrival_radius: float = 15.0,
    timeout_s: float = 30.0,
    poll_interval: float = 0.3,
) -> bool:
    """Poll /camera until player is within arrival_radius of target, or timeout."""
    deadline = time.time() + timeout_s
    while time.time() < deadline:
        try:
            px, py, _ = get_player_pos(base_url)
            d = dist2d((px, py), (target_x, target_y))
            if d <= arrival_radius:
                return True
        except Exception:
            pass
        time.sleep(poll_interval)
    return False


# ── main ─────────────────────────────────────────────────────────────────────

def main() -> int:
    ap = argparse.ArgumentParser(description="A* waypoint navigator for eq_client_lite")
    ap.add_argument("target", nargs="*", help="entity name or 'X Y' server coords")
    ap.add_argument("--entity", "-e", help="entity name to navigate to")
    ap.add_argument("--zone-id", type=int, metavar="ID",
                    help="navigate to zone exit for this zone ID (from /zone_points)")
    ap.add_argument("--map", nargs=2, type=float, metavar=("MAP_X", "MAP_Y"),
                    help="EQ map file coords")
    ap.add_argument("--port", type=int, default=8765)
    ap.add_argument("--cell-size", type=float, default=4.0)
    ap.add_argument("--z-band", type=float, default=200.0)
    ap.add_argument("--arrival", type=float, default=15.0)
    ap.add_argument("--assets", default=os.path.expanduser("~/eq_assets/EQ_Files"))
    ap.add_argument("--dry-run", action="store_true")
    args = ap.parse_args()

    base = f"http://localhost:{args.port}"

    # ── resolve start position ───────────────────────────────────────────────
    print("Getting player position...", file=sys.stderr)
    try:
        start_x, start_y, start_z = get_player_pos(base)
    except Exception as e:
        print(f"ERROR: cannot reach client at {base}: {e}", file=sys.stderr)
        return 1
    print(f"  start: server_x={start_x:.1f} server_y={start_y:.1f} z={start_z:.1f}",
          file=sys.stderr)

    # ── resolve goal position ────────────────────────────────────────────────
    goal_x = goal_y = goal_z = 0.0
    if args.zone_id is not None:
        result = get_zone_point(base, args.zone_id)
        if result is None:
            print(f"ERROR: no zone exit found for zone_id={args.zone_id} (try GET /zone_points)",
                  file=sys.stderr)
            return 1
        goal_x, goal_y, goal_z = result
        print(f"  zone exit for zone_id={args.zone_id}: ({goal_x:.1f}, {goal_y:.1f}, {goal_z:.1f})",
              file=sys.stderr)
    elif args.entity:
        result = get_entity_pos(base, args.entity)
        if result is None:
            print(f"ERROR: entity {args.entity!r} not found", file=sys.stderr)
            return 1
        goal_x, goal_y, goal_z = result
    elif args.map:
        # EQ map file: P map_x, map_y → server_x=-map_x, server_y=-map_y
        goal_x = -args.map[0]
        goal_y = -args.map[1]
        goal_z = start_z
    elif len(args.target) == 2:
        goal_x, goal_y = float(args.target[0]), float(args.target[1])
        goal_z = start_z
    elif len(args.target) == 1:
        result = get_entity_pos(base, args.target[0])
        if result is None:
            print(f"ERROR: entity {args.target[0]!r} not found", file=sys.stderr)
            return 1
        goal_x, goal_y, goal_z = result
    else:
        ap.print_help()
        return 1

    print(f"  goal:  server_x={goal_x:.1f} server_y={goal_y:.1f} z={goal_z:.1f}",
          file=sys.stderr)

    # ── quick bail if already close ───────────────────────────────────────────
    if dist2d((start_x, start_y), (goal_x, goal_y)) <= args.arrival:
        print("Already at destination.", file=sys.stderr)
        return 0

    # ── load zone navgrid ────────────────────────────────────────────────────
    zone = get_zone_name(base)
    if not zone:
        print("ERROR: could not determine current zone", file=sys.stderr)
        return 1
    s3d_path = os.path.join(args.assets, f"{zone}.s3d")
    print(f"Loading navgrid for zone '{zone}' ({s3d_path})...", file=sys.stderr)
    try:
        triangles = load_triangles(s3d_path)
    except FileNotFoundError:
        print(f"ERROR: S3D not found: {s3d_path}", file=sys.stderr)
        return 1
    print(f"  {len(triangles)} triangles", file=sys.stderr)

    grid = build_navgrid(triangles, cell_size=args.cell_size,
                         player_z=start_z, z_band=args.z_band)
    print(f"  grid: {grid.passable.shape[0]}×{grid.passable.shape[1]} cells "
          f"({grid.passable.sum()} passable)", file=sys.stderr)

    # ── pathfind ─────────────────────────────────────────────────────────────
    print("Running A*...", file=sys.stderr)
    raw_path = find_path(grid, (start_x, start_y), (goal_x, goal_y))
    if raw_path is None:
        print("ERROR: no path found (start or goal may be in impassable terrain)",
              file=sys.stderr)
        print("  Falling back to straight-line path.", file=sys.stderr)
        raw_path = [(goal_x, goal_y)]

    waypoints = simplify_path(raw_path, tolerance=5.0)
    print(f"  raw waypoints: {len(raw_path)}, simplified: {len(waypoints)}",
          file=sys.stderr)

    # ── print or execute ──────────────────────────────────────────────────────
    for i, (wx, wy) in enumerate(waypoints):
        print(f"  wp {i+1}/{len(waypoints)}: server_x={wx:.1f} server_y={wy:.1f}")

    if args.dry_run:
        print("Dry run — not navigating.", file=sys.stderr)
        return 0

    print("Navigating...", file=sys.stderr)
    for i, (wx, wy) in enumerate(waypoints):
        wz = goal_z if i == len(waypoints) - 1 else start_z
        print(f"  → wp {i+1}/{len(waypoints)}: ({wx:.1f}, {wy:.1f})", file=sys.stderr)
        if not post_goto(base, wx, wy, wz):
            print(f"  ERROR: /goto failed for waypoint {i+1}", file=sys.stderr)
            return 1
        arrived = wait_for_arrival(base, wx, wy, arrival_radius=args.arrival)
        if not arrived:
            print(f"  WARNING: timeout waiting for wp {i+1}, continuing...",
                  file=sys.stderr)

    print("Arrived at destination.", file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
