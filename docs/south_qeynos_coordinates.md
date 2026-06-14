# South Qeynos Coordinate System

## Key Finding: Axes Are Swapped

The user's map uses **(map_x, map_y)** where:

```
map_x = server_y   (server's Y axis = map's east-west axis)
map_y = server_x   (server's X axis = map's north-south axis)
```

### Evidence

| Location | Server (x, y) | Map (x, y) | Match |
|----------|--------------|------------|-------|
| Player spawn | (-202.6, 199.6) | (~200, ~-200) | server_y=199.6≈map_x=200 ✓, server_x=-202.6≈map_y=-200 ✓ |
| Barthal (DB spawn) | (-276, -32) | (~-55, ~-303) | server_y=-32≈map_x=-55 ✓*, server_x=-276≈map_y=-303 ✓* |

*Barthal wanders ~23-27 units from spawn point.

### Map Axis Directions

- Map X increases going **west** (positive = west, negative = east)
- Map Y increases going **north** (positive = north, negative = south)
- Map range: X from ~200 (west) to ~-600 (east), Y from ~600 (north) to ~-200 (south)

### Goto Conversion

To navigate to map coordinates (mx, my), send server coordinates (my, mx):

```bash
# Walk to map (-200, 200) [northeast]:
curl -X POST http://127.0.0.1:8765/goto -d '{"x": 200, "y": -200, "z": 3.75}'
#                                                  ^map_y    ^map_x
```

### HUD Display Fix Needed

Current HUD shows server (x, y). Should show map (server_y, server_x) so it matches the map.

---

## South Qeynos Landmarks

Map image: `south_qeynos_map.png`
Map coordinate labels: X=200 (west/left) to X=-600 (east/right), Y=600 (north/top) to Y=-200 (south/bottom)

| # | Location | Description |
|---|----------|-------------|
| 1 | Tin Soldier | Forge outside, Merchants selling Medium Chain Armor and Full Plate Molds |
| 2 | The Wind Spirit's Song | Bard Guild Hall, Merchants selling Bard songs and various Weapons |
| 3 | Fharn's Leather & Thread | Merchant selling Medium Leather Armor and Small Sewing Kit and Patterns |
| 4 | Bag n Barrel | Merchants selling Bags, Pottery Wheel and Kiln out back |
| 5 | Nesiff's Wooden Weapons | Merchants selling Blunt Weapons, Bows, outside Merchant selling Fletching Supplies; Royal Qeynos Forge nearby |
| 6 | Lion's Mane Inn | Merchants selling Alcohol, Brew Barrel, Message Board |
| 7 | Tax Hall | |
| 8 | Qeynos Hold | Bank |
| 9 | Underwater tunnel to Qeynos Aqueducts | |
| 10 | The Herb Jar | Merchants selling Spells, Potions, Books, Crystals, and Magician Equipment |
| 11 | Wizard/Enchanter/Magician Guild Hall | Merchants selling Spells and Wizard Equipment, outside Trainers |
| 12 | Tent Merchants | Selling Small Leather and Ringmail Armor and Medium Cloth Armor, Loom nearby |
| 13 | Fireprides | Merchants selling Medium Plate, Chain and Leather Armor and Shields, Shield Molds, Forge outside |
| 14 | Tent Merchant | Selling Large Leather and Ringmail Armor and Large Shields |
| 15 | Boat Dock | Travel to Erud's Crossing |
| 16 | Mermaid's Lure | Merchant selling Fishing Supplies |
| 17 | Tent Merchants | Selling Cloth Armor, Small Sewing Kits, Bags, Axes, and Sharp Weapons (including Claymore) |
| 18 | Warrior Training Hall | Inside Grounds of Fate (PvP Area), underground tunnel to Qeynos Aqueducts |
| 19 | Underwater tunnel to Qeynos Aqueducts | |
| 20 | Port Authority | |
| 21 | Merchant | Selling Instrument Parts, Spells, Compass, and Fish |
| 22 | Voleen's Fine Baked Goods | Merchants selling Food, Brewing Supplies, Baking Supplies, Oven inside |
| 23 | Fish's Ale | Merchants selling Alcohol, Brew Barrel inside, Message Board |
| 24 | Temple of Thunder | Paladin and Cleric Trainers, Merchants selling Spells, Weapons, and Shields |

**Royal Qeynos Forge**: Located next to the Clock Tower at approximately map (375, -365)
  → server coords: x = map_y = -365, y = map_x = 375 → server (-365, 375)
