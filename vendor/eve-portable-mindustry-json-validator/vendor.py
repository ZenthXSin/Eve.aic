#!/usr/bin/env python3
"""
Mindustry JSON Content Validator
模仿 ContentParser 的验证逻辑，传入 JSON 内容，输出报错和警告。
无需运行 Mindustry，纯 Python 实现。

用法:
    python3 mindustry_json_validator.py <file.json> [--type block|unit|item|liquid|status|planet|sector|weather|team]
    python3 mindustry_json_validator.py --help

参考源码: core/src/mindustry/mod/ContentParser.java (v8 / build 251)
"""

import json
import os
import re
import sys
import argparse
from dataclasses import dataclass, field
from enum import Enum
from typing import Any, Optional

# ============================================================
# 1. 内容类型枚举
# ============================================================
class ContentType(str, Enum):
    block = "block"
    unit = "unit"
    item = "item"
    liquid = "liquid"
    status = "status"
    planet = "planet"
    sector = "sector"
    weather = "weather"
    team = "team"

# ============================================================
# 2. ClassMap —— 源码 ClassMap.java 的完整映射 (简化类名 -> 全限定名)
# ============================================================
CLASS_MAP = {
    # AI
    "AssemblerAI": "mindustry.ai.types.AssemblerAI",
    "BoostAI": "mindustry.ai.types.BoostAI",
    "BuilderAI": "mindustry.ai.types.BuilderAI",
    "CargoAI": "mindustry.ai.types.CargoAI",
    "CommandAI": "mindustry.ai.types.CommandAI",
    "DefenderAI": "mindustry.ai.types.DefenderAI",
    "FlyingAI": "mindustry.ai.types.FlyingAI",
    "FlyingFollowAI": "mindustry.ai.types.FlyingFollowAI",
    "GroundAI": "mindustry.ai.types.GroundAI",
    "HugAI": "mindustry.ai.types.HugAI",
    "LogicAI": "mindustry.ai.types.LogicAI",
    "MinerAI": "mindustry.ai.types.MinerAI",
    "MissileAI": "mindustry.ai.types.MissileAI",
    "PrebuildAI": "mindustry.ai.types.PrebuildAI",
    "RepairAI": "mindustry.ai.types.RepairAI",
    "SuicideAI": "mindustry.ai.types.SuicideAI",
    # Abilities
    "Ability": "mindustry.entities.abilities.Ability",
    "ArmorPlateAbility": "mindustry.entities.abilities.ArmorPlateAbility",
    "EnergyFieldAbility": "mindustry.entities.abilities.EnergyFieldAbility",
    "ForceFieldAbility": "mindustry.entities.abilities.ForceFieldAbility",
    "LiquidExplodeAbility": "mindustry.entities.abilities.LiquidExplodeAbility",
    "LiquidRegenAbility": "mindustry.entities.abilities.LiquidRegenAbility",
    "MoveEffectAbility": "mindustry.entities.abilities.MoveEffectAbility",
    "MoveLightningAbility": "mindustry.entities.abilities.MoveLightningAbility",
    "RegenAbility": "mindustry.entities.abilities.RegenAbility",
    "RepairFieldAbility": "mindustry.entities.abilities.RepairFieldAbility",
    "ShieldArcAbility": "mindustry.entities.abilities.ShieldArcAbility",
    "ShieldRegenFieldAbility": "mindustry.entities.abilities.ShieldRegenFieldAbility",
    "SpawnDeathAbility": "mindustry.entities.abilities.SpawnDeathAbility",
    "StatusFieldAbility": "mindustry.entities.abilities.StatusFieldAbility",
    "SuppressionFieldAbility": "mindustry.entities.abilities.SuppressionFieldAbility",
    "UnitSpawnAbility": "mindustry.entities.abilities.UnitSpawnAbility",
    # Bullet types
    "ArtilleryBulletType": "mindustry.entities.bullet.ArtilleryBulletType",
    "BasicBulletType": "mindustry.entities.bullet.BasicBulletType",
    "BombBulletType": "mindustry.entities.bullet.BombBulletType",
    "BulletType": "mindustry.entities.bullet.BulletType",
    "ContinuousBulletType": "mindustry.entities.bullet.ContinuousBulletType",
    "ContinuousFlameBulletType": "mindustry.entities.bullet.ContinuousFlameBulletType",
    "ContinuousLaserBulletType": "mindustry.entities.bullet.ContinuousLaserBulletType",
    "EmpBulletType": "mindustry.entities.bullet.EmpBulletType",
    "EmptyBulletType": "mindustry.entities.bullet.EmptyBulletType",
    "ExplosionBulletType": "mindustry.entities.bullet.ExplosionBulletType",
    "FireBulletType": "mindustry.entities.bullet.FireBulletType",
    "FlakBulletType": "mindustry.entities.bullet.FlakBulletType",
    "InterceptorBulletType": "mindustry.entities.bullet.InterceptorBulletType",
    "LaserBoltBulletType": "mindustry.entities.bullet.LaserBoltBulletType",
    "LaserBulletType": "mindustry.entities.bullet.LaserBulletType",
    "LightningBulletType": "mindustry.entities.bullet.LightningBulletType",
    "LiquidBulletType": "mindustry.entities.bullet.LiquidBulletType",
    "MassDriverBolt": "mindustry.entities.bullet.MassDriverBolt",
    "MissileBulletType": "mindustry.entities.bullet.MissileBulletType",
    "MultiBulletType": "mindustry.entities.bullet.MultiBulletType",
    "PointBulletType": "mindustry.entities.bullet.PointBulletType",
    "PointLaserBulletType": "mindustry.entities.bullet.PointLaserBulletType",
    "RailBulletType": "mindustry.entities.bullet.RailBulletType",
    "SapBulletType": "mindustry.entities.bullet.SapBulletType",
    "ShrapnelBulletType": "mindustry.entities.bullet.ShrapnelBulletType",
    "SpaceLiquidBulletType": "mindustry.entities.bullet.SpaceLiquidBulletType",
    # Effects
    "ExplosionEffect": "mindustry.entities.effect.ExplosionEffect",
    "MultiEffect": "mindustry.entities.effect.MultiEffect",
    "ParticleEffect": "mindustry.entities.effect.ParticleEffect",
    "RadialEffect": "mindustry.entities.effect.RadialEffect",
    "SeqEffect": "mindustry.entities.effect.SeqEffect",
    "SoundEffect": "mindustry.entities.effect.SoundEffect",
    "WaveEffect": "mindustry.entities.effect.WaveEffect",
    "WrapEffect": "mindustry.entities.effect.WrapEffect",
    # DrawParts
    "DrawPart": "mindustry.entities.part.DrawPart",
    "EffectSpawnerPart": "mindustry.entities.part.EffectSpawnerPart",
    "FlarePart": "mindustry.entities.part.FlarePart",
    "HaloPart": "mindustry.entities.part.HaloPart",
    "HoverPart": "mindustry.entities.part.HoverPart",
    "RegionPart": "mindustry.entities.part.RegionPart",
    "ShapePart": "mindustry.entities.part.ShapePart",
    # Shoot patterns
    "ShootAlternate": "mindustry.entities.pattern.ShootAlternate",
    "ShootBarrel": "mindustry.entities.pattern.ShootBarrel",
    "ShootHelix": "mindustry.entities.pattern.ShootHelix",
    "ShootMulti": "mindustry.entities.pattern.ShootMulti",
    "ShootPattern": "mindustry.entities.pattern.ShootPattern",
    "ShootSine": "mindustry.entities.pattern.ShootSine",
    "ShootSpread": "mindustry.entities.pattern.ShootSpread",
    "ShootSummon": "mindustry.entities.pattern.ShootSummon",
    # Objectives
    "Objective": "mindustry.game.Objectives.Objective",
    "OnPlanet": "mindustry.game.Objectives.OnPlanet",
    "OnSector": "mindustry.game.Objectives.OnSector",
    "Produce": "mindustry.game.Objectives.Produce",
    "Research": "mindustry.game.Objectives.Research",
    "SectorComplete": "mindustry.game.Objectives.SectorComplete",
    # Type
    "Category": "mindustry.type.Category",
    "Item": "mindustry.type.Item",
    "ItemStack": "mindustry.type.ItemStack",
    "Liquid": "mindustry.type.Liquid",
    "LiquidStack": "mindustry.type.LiquidStack",
    "PayloadStack": "mindustry.type.PayloadStack",
    "Planet": "mindustry.type.Planet",
    "SectorPreset": "mindustry.type.SectorPreset",
    "StatusEffect": "mindustry.type.StatusEffect",
    "TeamEntry": "mindustry.type.TeamEntry",
    "UnitType": "mindustry.type.UnitType",
    "UnitEngine": "mindustry.type.UnitType.UnitEngine",
    "Weapon": "mindustry.type.Weapon",
    "Weather": "mindustry.type.Weather",
    # UnitTypes
    "ErekirUnitType": "mindustry.type.unit.ErekirUnitType",
    "MissileUnitType": "mindustry.type.unit.MissileUnitType",
    "NeoplasmUnitType": "mindustry.type.unit.NeoplasmUnitType",
    "TankUnitType": "mindustry.type.unit.TankUnitType",
    # Weapons
    "BuildWeapon": "mindustry.type.weapons.BuildWeapon",
    "MineWeapon": "mindustry.type.weapons.MineWeapon",
    "PointDefenseWeapon": "mindustry.type.weapons.PointDefenseWeapon",
    "PointDefenseBulletWeapon": "mindustry.type.weapons.PointDefenseBulletWeapon",
    "RepairBeamWeapon": "mindustry.type.weapons.RepairBeamWeapon",
    # Weather
    "MagneticStorm": "mindustry.type.weather.MagneticStorm",
    "ParticleWeather": "mindustry.type.weather.ParticleWeather",
    "RainWeather": "mindustry.type.weather.RainWeather",
    "SolarFlare": "mindustry.type.weather.SolarFlare",
    # Blocks (common types)
    "Block": "mindustry.world.Block",
    "GenericCrafter": "mindustry.world.blocks.crafting.GenericCrafter",
    "AdvancedGenericCrafter": "mindustry.world.blocks.crafting.AdvancedGenericCrafter",
    "LiquidConverter": "mindustry.world.blocks.crafting.LiquidConverter",
    "Reactor": "mindustry.world.blocks.power.Reactor",
    "BurnerGenerator": "mindustry.world.blocks.power.BurnerGenerator",
    "SolarGenerator": "mindustry.world.blocks.power.SolarGenerator",
    "ThermalGenerator": "mindustry.world.blocks.power.ThermalGenerator",
    "ImpactReactor": "mindustry.world.blocks.power.ImpactReactor",
    "ItemTurret": "mindustry.world.blocks.defense.turrets.ItemTurret",
    "PowerTurret": "mindustry.world.blocks.defense.turrets.PowerTurret",
    "LiquidTurret": "mindustry.world.blocks.defense.turrets.LiquidTurret",
    "ContinuousTurret": "mindustry.world.blocks.defense.turrets.ContinuousTurret",
    "ContinuousLiquidTurret": "mindustry.world.blocks.defense.turrets.ContinuousLiquidTurret",
    "LaserTurret": "mindustry.world.blocks.defense.turrets.LaserTurret",
    "Turret": "mindustry.world.blocks.defense.turrets.Turret",
    "BaseTurret": "mindustry.world.blocks.defense.turrets.BaseTurret",
    "ReloadTurret": "mindustry.world.blocks.defense.turrets.ReloadTurret",
    "PointDefenseTurret": "mindustry.world.blocks.defense.turrets.PointDefenseTurret",
    "TractorBeamTurret": "mindustry.world.blocks.defense.turrets.TractorBeamTurret",
    "PayloadAmmoTurret": "mindustry.world.blocks.defense.turrets.PayloadAmmoTurret",
    "BuildTurret": "mindustry.world.blocks.defense.BuildTurret",
    "Wall": "mindustry.world.blocks.defense.Wall",
    "ShieldWall": "mindustry.world.blocks.defense.ShieldWall",
    "Door": "mindustry.world.blocks.defense.Door",
    "AutoDoor": "mindustry.world.blocks.defense.AutoDoor",
    "ForceProjector": "mindustry.world.blocks.defense.ForceProjector",
    "MendProjector": "mindustry.world.blocks.defense.MendProjector",
    "OverdriveProjector": "mindustry.world.blocks.defense.OverdriveProjector",
    "BaseShield": "mindustry.world.blocks.defense.BaseShield",
    "ShockwaveTower": "mindustry.world.blocks.defense.ShockwaveTower",
    "DirectionalForceProjector": "mindustry.world.blocks.defense.DirectionalForceProjector",
    "Radar": "mindustry.world.blocks.defense.Radar",
    "RegenProjector": "mindustry.world.blocks.defense.RegenProjector",
    "Conveyor": "mindustry.world.blocks.distribution.Conveyor",
    "ArmoredConveyor": "mindustry.world.blocks.distribution.ArmoredConveyor",
    "Duct": "mindustry.world.blocks.distribution.Duct",
    "DuctBridge": "mindustry.world.blocks.distribution.DuctBridge",
    "DuctJunction": "mindustry.world.blocks.distribution.DuctJunction",
    "DirectionBridge": "mindustry.world.blocks.distribution.DirectionBridge",
    "DirectionalUnloader": "mindustry.world.blocks.distribution.DirectionalUnloader",
    "BufferedItemBridge": "mindustry.world.blocks.distribution.BufferedItemBridge",
    "ItemBridge": "mindustry.world.blocks.distribution.ItemBridge",
    "Sorter": "mindustry.world.blocks.distribution.Sorter",
    "Router": "mindustry.world.blocks.distribution.Router",
    "MassDriver": "mindustry.world.blocks.distribution.MassDriver",
    "ConsumeGenerator": "mindustry.world.blocks.power.ConsumeGenerator",
    "DifferentialGenerator": "mindustry.world.blocks.power.DifferentialGenerator",
    "TurbineGenerator": "mindustry.world.blocks.power.TurbineGenerator",
    "SolarPanel": "mindustry.world.blocks.power.SolarPanel",
    "ThermoelectricGenerator": "mindustry.world.blocks.power.ThermoelectricGenerator",
    "NuclearReactor": "mindustry.world.blocks.power.NuclearReactor",
    "RTGGenerator": "mindustry.world.blocks.power.RTGGenerator",
    "Battery": "mindustry.world.blocks.power.Battery",
    "PowerNode": "mindustry.world.blocks.power.PowerNode",
    "PowerWire": "mindustry.world.blocks.power.PowerWire",
    "LiquidTank": "mindustry.world.blocks.liquid.LiquidTank",
    "LiquidRouter": "mindustry.world.blocks.liquid.LiquidRouter",
    "LiquidJunction": "mindustry.world.blocks.liquid.LiquidJunction",
    "LiquidBridge": "mindustry.world.blocks.liquid.LiquidBridge",
    "LiquidPipe": "mindustry.world.blocks.liquid.LiquidPipe",
    "LiquidValve": "mindustry.world.blocks.liquid.LiquidValve",
    "LiquidEndpoint": "mindustry.world.blocks.liquid.LiquidEndpoint",
    "Conduit": "mindustry.world.blocks.liquid.Conduit",
    "PressureConduit": "mindustry.world.blocks.liquid.PressureConduit",
    "PulseConduit": "mindustry.world.blocks.liquid.PulseConduit",
    "PhaseConduit": "mindustry.world.blocks.liquid.PhaseConduit",
    "UnitFactory": "mindustry.world.blocks.units.UnitFactory",
    "Reconstructor": "mindustry.world.blocks.units.Reconstructor",
    "RepairPoint": "mindustry.world.blocks.units.RepairPoint",
    "UnitAssembler": "mindustry.world.blocks.units.UnitAssembler",
    "UnitAssemblerDriver": "mindustry.world.blocks.units.UnitAssemblerDriver",
    "PayloadLoader": "mindustry.world.blocks.payloads.PayloadLoader",
    "PayloadUnloader": "mindustry.world.blocks.payloads.PayloadUnloader",
    "PayloadConveyor": "mindustry.world.blocks.payloads.PayloadConveyor",
    "PayloadRouter": "mindustry.world.blocks.payloads.PayloadRouter",
    "PayloadNode": "mindustry.world.blocks.payloads.PayloadNode",
    "LaunchPad": "mindustry.world.blocks.campaign.LaunchPad",
    "LandingPad": "mindustry.world.blocks.campaign.LandingPad",
    "Accelerator": "mindustry.world.blocks.campaign.Accelerator",
    "Core": "mindustry.world.blocks.storage.Core",
    "CoreShard": "mindustry.world.blocks.storage.Core",
    "CoreFoundation": "mindustry.world.blocks.storage.Core",
    "CoreNucleus": "mindustry.world.blocks.storage.Core",
    "StorageBlock": "mindustry.world.blocks.storage.StorageBlock",
    "Vault": "mindustry.world.blocks.storage.Vault",
    "Container": "mindustry.world.blocks.storage.Container",
    "Unloader": "mindustry.world.blocks.storage.Unloader",
    "VoidBlock": "mindustry.world.blocks.storage.VoidBlock",
    "StorageFloor": "mindustry.world.blocks.storage.StorageFloor",
    "BuildBlock": "mindustry.world.blocks.BuildBlock",
    "ConstructBlock": "mindustry.world.blocks.ConstructBlock",
    "Placeholder": "mindustry.world.blocks.Placeholder",
    "BedrockBlock": "mindustry.world.blocks.BedrockBlock",
    "OreBlock": "mindustry.world.blocks.OreBlock",
    "Floor": "mindustry.world.blocks.Floor",
    "StaticWall": "mindustry.world.blocks.StaticWall",
    "Cliff": "mindustry.world.blocks.Cliff",
    "AirBlock": "mindustry.world.blocks.AirBlock",
    "SeaBlock": "mindustry.world.blocks.SeaBlock",
    "OverlayFloor": "mindustry.world.blocks.OverlayFloor",
    "Interp": "arc.math.Interp",
    "Blending": "arc.graphics.Blending",
    "CacheLayer": "mindustry.graphics.CacheLayer",
    "BuildVisibility": "mindustry.world.BuildVisibility",
    "DrawDefault": "mindustry.world.draw.DrawDefault",
    "DrawMulti": "mindustry.world.draw.DrawMulti",
    "DrawRegion": "mindustry.world.draw.DrawRegion",
    "DrawIcon": "mindustry.world.draw.DrawIcon",
    "DrawHeat": "mindustry.world.draw.DrawHeat",
    "DrawGlow": "mindustry.world.draw.DrawGlow",
    "DrawParticles": "mindustry.world.draw.DrawParticles",
    "DrawLiquid": "mindustry.world.draw.DrawLiquid",
    "DrawLiquidTop": "mindustry.world.draw.DrawLiquidTop",
    "DrawFlame": "mindustry.world.draw.DrawFlame",
    "DrawCrucibleFlame": "mindustry.world.draw.DrawCrucibleFlame",
    "DrawPower": "mindustry.world.draw.DrawPower",
    "DrawBlock": "mindustry.world.draw.DrawBlock",
}

# ============================================================
# 3. Vanilla content name registries
# ============================================================
# These are extracted from Content.*.java files
# Format: set of names (without mod prefix)

VANILLA_ITEMS = {
    "copper", "lead", "graphite", "coal", "titanium", "thorium", "silicon",
    "plastanium", "phase-fabric", "surge-alloy", "spore-pod", "sand",
    "blast-compound", "pyratite", "metaglass", "scrap", "carbon",
    "berylicium", "tungsten", "titan", "carbide", "beryl", "crystalline",
    "arkycite", "neoplasm", "fissile-matter", "calene", "xenon",
}

VANILLA_LIQUIDS = {
    "water", "slag", "oil", "cryofluid", "plasma", " slag",
    "hydrogen", "helium", "arkycite", "arkycite-fissile",
    "cyanogen", "ethanol", "nitrogen", "hydrogen",
}

VANILLA_BLOCKS = {
    # Floors / terrain
    "air", "empty", "spawn", "deepwater", "water", "tainted-water",
    "taintedwater", "sandwater", "darksandwater", "tar",
    "stone", "craters", "charr", "basalt", "hotrock", "magmarock",
    "sand", "darksand", "dirt", "mud", "dacite", "grass", "salt",
    "snow", "ice", "icesnow", "rhyolite", "regolith",
    "carbonstone", "beryllicstone", "crystallinestone", "crystalfloor",
    # Walls
    "stonewall", "basaltwall", "granitewall", "carbonwall",
    "ferricwall", "beryllicwall", "tungsten-wall", "thorium-wall",
    "phase-wall", "dark-graphite-wall",
    # Distribution
    "conveyor", "armored-conveyor", "titanium-conveyor", "plastanium-conveyor",
    "phase-conveyor", "router", "distributor", "sorter", "inverted-sorter",
    "overflow-gate", "underflow-gate", "junction", "item-bridge",
    "mass-driver", "duct", "armored-duct", "duct-bridge", "duct-junction",
    # Liquid
    "conduit", "pulse-conduit", "phase-conduit", "pressured-conduit",
    "liquid-router", "liquid-tank", "liquid-junction", "liquid-bridge",
    "liquid-endpoint", "rotary-liquid-conduit", "liquid-valve",
    "liquid-source", "liquid-void",
    # Defense
    "copper-wall", "titanium-wall", "phase-wall", "thorium-wall",
    "door", "large-door",
    "duo", "scatter", "hail", "wave", "salvo", "arc",
    "titanium-turret", "lynx", "ripple", "cyclone", "spectre",
    "meltdown", "foreshadow",
    "parallax", "segment",
    "force-projector", "mend-projector", "overdrive-projector",
    "overdrive-dome", "force-shield",
    # Crafting
    "graphite-pressor", "silicon-smelter", "kiln", "pluralizer",
    "pulverizer", "coal-centrifuge", "incinerator",
    "copper-smelter", "titanium-smelter", "thorium-smelter",
    "metaglass-furnace", "plastanium-compressor",
    "phase-weaver", "surge-smelter", "pyratite-mixer", "blast-mixer",
    "cryofluid-mixer", "blast-compound-mixer",
    "silicon-crucible", "plasma-bore", "large-plasma-bore",
    "beam-drill", "large-beam-drill",
    "water-extractor", "cultivator",
    # Power
    "power-node", "large-power-node", "power-source", "battery",
    "large-battery", "solar-panel", "differential-generator",
    "rtg-generator", "thermal-generator", "steam-generator",
    "impact-reactor", "thorium-reactor",
    # Production
    "mechanical-drill", "pneumatic-drill", "laser-drill",
    "laser-junction", "beam-router",
    # Units
    "ground-factory", "air-factory", "naval-factory",
    "mechanical-factory", "additive-reconstructor",
    "multiplicative-reconstructor", "exponential-reconstructor",
    "tetrative-reconstructor",
    # Storage
    "core-shard", "core-foundation", "core-nucleus",
    "vault", "container", "unloader", "large-unloader",
    "launch-pad", "interplanetary-launch-pad",
    # Campaign
    "launch-pad", "landing-pad", "accelerator",
    # Payload
    "payload-conveyor", "payload-router", "payload-loader",
    "payload-unloader",
    # Sandbox
    "item-source", "item-void", "liquid-source", "liquid-void",
    "power-source", "power-void",
    # Erekir-specific
    "tungsten-wall-large", "beryllium-wall", "beryllium-wall-large",
    "reinforced-wall", "reinforced-wall-large", "reinforced-door",
    "large-reinforced-door",
    "breach", "sublimate", "desolate", "glaive",
    "disperse", "sporula", "ripole",
    "regulator", "atomic-dissolve",
    "surge-tower", "diode",
    "beam-node", "beam-tower",
    "impact-driver", "turbine-condenser",
    "chemical-combustion-chamber", "pyrolysis-generator",
    "flux-reactor", "neoplasia-reactor",
    "unit-repair-tower", "regen-projector",
    "world-cell", "payload-reconstructor", "large.payload-reconstructor",
    "tank-fabricator", "tank-reconstructor",
    "mech-fabricator", "mech-reconstructor",
    "ship-fabricator", "ship-reconstructor",
    "tank-refabricator", "mech-refabricator", "ship-refabricator",
    "mechanical-core", "afflict",
    "unit-factory", "large-unit-factory",
    "assembler-module", "assembler-module-large",
    "unit-assembler", "unit-assembler-driver",
    "payload-assembler", "payload-loadout",
    "payload-unload-point", "payload-conveyor", "payload-router",
    "payload-mass-driver",
    # Router / misc
    "router", "conveyor",
}

VANILLA_UNITS = {
    "dagger", "mace", "flak-fortress", "scepter", "nova",
    "horizon", "zenith", "antumbra", "eclipse",
    "glaive", "poly", "mega", "javelin", "quell", "disrupt",
    "mono", "poly", "mega",
    "risso", "minke", "bryde", "leviathan", "omura",
    "alpha", "beta", "gamma",
    # Erekir
    "stell", "crease", "locus", "assembly", "conquer",
    "vanquish", "precept", "prophet",
    "evoke", "incite", "emanate",
}

VANILLA_WEATHERS = {
    "rain", "snow", "sandstorms", "spore-storms",
    "particle-thickfog", "fog",
}

VANILLA_PLANETS = {
    "serpulo", "erekir",
}

VANILLA_STATUS_EFFECTS = {
    "none", "burning", "melting", "freezing", "wet", "metaglassed",
    "slow", "unmoving", "fast", "shocked", "slowed",
    "tarred", "overclock", "overdrive", "overclockfield",
    "powerioverdrive", "slow-field", "tar-field", "spore-slow",
    "blasted", "bren", "brened",
}

VANILLA_TEAMS = {
    "derelict", "sharded", "crux", "malis", "green",
    "blue", "purple", "blu", "blue-team",
}

# ============================================================
# 4. Known field names for common classes (subset that matters for validation)
# ============================================================

# Fields commonly found on Block and its subclasses
# (not exhaustive, but covers the most common mistakes)
BLOCK_FIELDS = {
    "type", "name", "description", "localizedName", "requirements",
    "health", "size", "solid", "update", "sync", "compress",
    "debris", "hasShadow", "clipSize", "allowLiquidRendering",
    "envDisabled", "envRequired", "envPropensity", "envSignificance",
    "buildType", "hasBuilding", "destroyBulletEffect", "destroyBullet",
    "inaccuracy", "velocityInaccuracy", "reload", "range",
    "recoil", "recoilPad", "size", "placeOverlapMargin",
    "customShadow", "swapOverride", "forceSwap",
    "playerOffset", "coreMerge", "coreShape",
    "lightRadius", "lightType", "lightColor", "lightOpacity",
    "ambientLightColor", "ambientLight",
    # Categories/visibility
    "buildVisibility", "category", "group",
    # Draw
    "drawer", "draw", "drawType",
    # Research
    "research",
    # Consumers
    "consumes",
    # Build checks
    "canOverdrive", "canReplace", "keepBuildings",
    "hasItems", "hasLiquids", "hasPower",
    "itemCapacity", "liquidCapacity", "powerCapacity",
    "itemDuration", "liquidPressure",
    # Output
    "outputItems", "outputLiquids", "outputPayloads",
    # Optional visuals
    "squareSprite", "floating", "underBullets",
    "placeEffect", "breakEffect", "destroyEffect",
    "topEffect", "updateEffect", "updateEffectChance",
    "setEffect", "setPatientsEffect",
    "outlineColor", "outlineRadius", "outlineIcon",
    # Block config
    "health", "armor", "hitSize",
    "solid", "fogRadius", "team",
    "hasShadow", "insulated", "conductivity",
    "timers", "clipSize",
    "color",  # @Nullable Color color on Block
    # Common subclass fields (merged from Block subclasses to reduce false positives)
    # MendProjector
    "healPercent", "healAmount", "useTime", "phaseBoost", "phaseRangeBoost",
    "repairSpeed", "useNonTier",
    # ForceProjector / DomeProjector
    "radius", "phaseRadiusBoost", "phaseShieldBoost", "phaseUseTime",
    "breakageEffect", "regenEffect", "absorbEffect", "shieldEffect",
    "shieldWidth", "shieldLength", "shieldHealth",
    "cooldownNormal", "cooldownLiquid", "cooldownBrokenBase", "padSize",
    "width", "length",
    # OverdriveProjector / OverdriveDome
    "speedBoost", "speedBoostPhase", "baseSpeed", "hasBoost", "rangeBoost",
    # ShockwaveTower
    "bulletDamage", "falloffCount", "shake", "checkInterval",
    "cooldownMultiplier", "shapeRotateSpeed", "shapeRadius", "shapeSides",
    # Power
    "powerProduction", "powerOutput", "laserRange", "maxNodes", "pulseEffect",
    "generateTime", "initialGenerationTime", "powerMultiplier", "basePowerStorage",
    "fuelUseTime", "heating", "heatOutput", "efficiency", "heatsPerTick",
    # Distribution / misc
    "speed", "bufferCapacity", "liquidSpeed", "liquidCapacity",
    "toggleable", "allowSelfLoop",
    # Drill
    "drillTime", "drillEffect",
    # Crafting
    "craftEffect", "crafter",
    # LaunchPad / Payload
    "launchTime", "launchEffect", "payloadSpeed", "payloadCapacity",
    "maxBlockSize", "maxBuildTime",
    # Generator
    "produceTime",
    # Turret
    "shootType", "shootY", "shootX", "shootZ",
    # UnitFactory / Reconstructor
    "plans", "unitType", "upgrades",
}

TURRET_FIELDS = {
    "type", "name", "health", "size", "reload", "range",
    "ammoTypes", "ammo", "shoot", "velocityInaccuracy",
    "inaccuracy", "shots", "burstSpacing", "shootY",
    "recoil", "recoilTime", "recoilAmount", "recoilPad",
    "coolant", "consumeAmmoPerShot",
    "canOverdrive", "canRotate", "rotateSpeed",
    "targetInterval", "targetSwitchInterval",
    "sound", "soundVolume", "shootSound",
    "ejectEffect", "destroySound",
    "layer", "bulletOffset", "drawer",
    "heatColor", "heatColor",
    # Continuous turret
    "beamWidth", "beamLength", "beamMinLength",
    "continuous",
    "power", "coolant",
}

UNIT_TYPE_FIELDS = {
    "type", "name", "health", "speed", "hitSize", "range",
    "flying", "lowAltitude", "engineOffset", "engineSize",
    "boostMultiplier", "drillSize", "mineSpeed",
    "buildSpeed", "drownTimeMultiplier",
    "itemCapacity", "liquidCapacity",
    "meleeRange", "meleeDamage", "meleeDelay",
    "faceTarget", "isEnemy", "playerControllable",
    "canBeCommandable", "commandLimit",
    "deathSound", "hitSound", "explodeSound",
    "deathExplosionEffect", "hitEffect", "fallEffect",
    "riseEffect", "trailEffect",
    "weapons", "abilities",
    "parts", "engineEffects",
    "legCount", "legLength", "legSpeed", "legSpeedMultiplier",
    "legSpread", "legStraight", "legPairOffset",
    "lockLegBase", "legContinuousMove",
    "floating", "swimSpeed",
    "payloadCapacity", "payloadOffset",
    "drawCell", "drawItems", "drawShields",
    "setFloor", "envRequired",
    "armor", "mass",
    "aiController", "controller", "defaultController",
    "requireClientVelocity",
    "waves", "waveSpacing",
    "visualElevation",
    "groundLayer",
    "outlineColor", "outlineRadius",
    "requirements",  # special: parsed into factory/reconstructor plans by ContentParser
    "template",  # special: base UnitType to inherit from
}

WEAPON_FIELDS = {
    "type", "name", "bullet", "reload", "range",
    "recoil", "recoilTime", "recoilAmount", "recoilPad",
    "shootX", "shootY", "shootZ",
    "inaccuracy", "velocityInaccuracy",
    "shots", "burstSpacing", "shotDelay",
    "shootEffect", "smokeEffect", "ejectEffect",
    "sound", "soundVolume", "shootSound",
    "soundPitchMin", "soundPitchMax",
    "x", "y", "z", "mirror", "top", "under",
    "rotate", "fixed", "heatColor",
    "layers", "parts",
    "minRange", "targetInterval", "targetSwitchInterval",
    "continuous", "predictTarget",
    "ignoreRotation", "alwaysShooting",
    "draw", "drawer",
    "useAmmo", "consumeAmmo",
    "optional", "alwaysActive",
    "moveX", "moveY",
}

BULLET_TYPE_FIELDS = {
    "type", "damage", "speed", "lifetime", "width", "height",
    "hitEffect", "despawnEffect", "shootEffect",
    "smokeEffect", "hitSound", "despawnSound",
    "hitSize", "pierce", "pierceCap",
    "homingPower", "homingRange",
    "weaveScale", "weaveMag",
    "lightColor", "lightRadius",
    "lightOpacity",
    "fragBullet", "fragBullets", "fragVelocityMin", "fragVelocityMax",
    "fragLifeMin", "fragLifeMax",
    "splashDamage", "splashRange",
    "knockback", "hitKnockback",
    "buildingDamageMultiplier",
    "impactX", "impactY",
    "trailColor", "trailWidth", "trailLength",
    "backColor", "frontColor",
    "mixColorFrom", "mixColorTo",
    "status", "statusDuration",
    "incendAmount", "incendChance", "incendSpread",
    "fireEffect",
    "makeFire",
    "hittable", "absorbable",
    "keepVelocity",
    "detonateFullscreen",
    "shrinkX", "shrinkY",
    "hitShake", "hitFlare",
    "magazine", "shootEffect", "smokeEffect",
    "landEffect",
    "tumbleEffect",
    "spin", "spinBounds",
    "orbital",
}

# DrawBlock known types
DRAWBLOCK_TYPES = {
    "DrawDefault", "DrawMulti", "DrawRegion", "DrawIcon",
    "DrawHeat", "DrawGlow", "DrawParticles", "DrawLiquid",
    "DrawLiquidTop", "DrawFlame", "DrawCrucibleFlame", "DrawPower",
    "DrawShape", "DrawBlur", "DrawBlock", "DrawTeam",
    "DrawCustom", "DrawFade", "DrawGrowth", "DrawOverlays",
    "DrawPlan", "DrawPlanRegion", "DrawString", "DrawSynapse",
}

# Known effects (from Fx class) — a subset of common ones
KNOWN_FX = {
    "none", "spark", "bigCone", "trailSmoke", "shieldBreak",
    "blockExplode", "blockBreak", "dynamicExplosion", "explosion",
    "torch", "flicker", "bubbles", "fire", "smoke", "steam",
    "place", "placeBlock", "breakBlock", "placeGen",
    "lightningCharge", "lightning", "lightningHit",
    "laser", "laserHit", "lancerLaserShoot", "lancerLaserCharge",
    "chainLightning", "chain", "heal", "healWave",
    "doorOpen", "doorClose",
    "deposit", "loadPayload", "unloadPayload",
    "pickUp", "drop",
    "turbineOn", "generatorOn", "reactorOn", "reactorStop",
    "coreCharging", "coreExplosion",
    "unitSpawn", "unitCap",
    "trail", "missileTrail", "rocketTrail",
    "wingSmoke", "wingTrail",
    "shot", "shotSmall", "shotBig", "shotHuge",
    "shootLarge", "shootSmall",
    "railShoot", "railHit",
    "padShoot",
    "forceRecoil",
    "overpressured",
    "shockwave", "shockwaveSlow", "largeShockwave",
    "blast", "massiveBlast", "nuclearBlast",
    "vapor", "wet", "wetGround",
    "bubble", "rubble",
    "drillCharge", "drillShoot", "drillPress",
    "consume", "consumePower", "flame", "smeltsmoke",
    "forming", "formingBeam", "formingSmelt",
    "fuel",
    "pair", "pairHit",
    "teleport", "teleportActivate", "teleportOut",
    "hitLaser", "hitBeam", "hitSmallLaser",
    "crossBlast", "massive", "nuclearcloud",
    "death", "pulverize", "mine",
    "sapping", "sapperShoot",
    "sign", "warning",
}

# Sound names (from Sounds class) — subset
KNOWN_SOUNDS = {
    "none", "place", "break", "explosion", "bigExplosion",
    "rocket", "rocketBig", "shock", "laser", "lasercharge",
    "laserShoot", "smelter", "fire", "fireWave",
    "door", "doorOpen", "doorClose",
    "click", "unlock", "buttonClick", "back",
    "craft", "drop", "pickup",
    "build", "buildBig", "deconstruct",
    "shot", "shotgun", "shotHevy", "railgun", "railgun2",
    "cannon", "artillery",
    "pulse", "buzz", "electricHum", "electricSpark",
    "lightning", "lightning3",
    "shield", "shieldUp", "shieldDown",
    "heal", "healBeam", "healCharge",
    "core1", "core2", "core3", "core4", "core5",
    "respawn", "taken", "warn",
    "wind", "rain", "blizzard",
    "mud", "swamp", "snow",
    "missile", "missileSmall",
    "airblast", "claw", "release",
    "flame", "flame2", "flameen",
    "spray", "sprayFlame",
    "mine", "mining", "miningLarge",
    "conveyor", "conveyorBelt",
    "melter", "freezer", "beam",
    "spore", "blast", "pyratite",
}

# ============================================================
# 5. Allowed unit types (from ContentParser.unitType)
# ============================================================
ALLOWED_UNIT_TYPES = {
    "flying", "mech", "legs", "naval", "payload",
    "missile", "tank", "hover", "tether", "crawl",
}

# ============================================================
# 6. Validation result types
# ============================================================
@dataclass
class ValidationMessage:
    level: str  # "ERROR" or "WARN"
    message: str
    path: str = ""  # JSON path where issue occurred

    def __str__(self):
        prefix = f"[{self.level}]"
        loc = f" (at {self.path})" if self.path else ""
        return f"{prefix} {self.message}{loc}"

@dataclass
class ValidationResult:
    messages: list = field(default_factory=list)
    
    def error(self, msg: str, path: str = ""):
        self.messages.append(ValidationMessage("ERROR", msg, path))
    
    def warn(self, msg: str, path: str = ""):
        self.messages.append(ValidationMessage("WARN", msg, path))
    
    @property
    def has_errors(self):
        return any(m.level == "ERROR" for m in self.messages)
    
    @property
    def has_warnings(self):
        return any(m.level == "WARN" for m in self.messages)
    
    @property
    def error_count(self):
        return sum(1 for m in self.messages if m.level == "ERROR")
    
    @property
    def warn_count(self):
        return sum(1 for m in self.messages if m.level == "WARN")


# ============================================================
# 7. Main Validator class
# ============================================================
class ContentValidator:
    """
    Mindustry JSON Content Validator
    Mirrors the validation logic of ContentParser.java
    """
    
    def __init__(self, mod_name: str = "mod", content_type: Optional[ContentType] = None):
        self.mod_name = mod_name
        self.explicit_content_type = content_type
        self.result = ValidationResult()
        self.custom_content: set = set()  # 来自同一批次的自定义内容名
    
    def validate_file(self, filepath: str) -> ValidationResult:
        """验证单个 JSON 文件"""
        self.result = ValidationResult()
        
        # 检测内容类型
        content_type = self.explicit_content_type or self._detect_type_from_path(filepath)
        
        # 读取文件
        try:
            with open(filepath, 'r', encoding='utf-8') as f:
                raw = f.read()
        except FileNotFoundError:
            self.result.error(f"File not found: {filepath}")
            return self.result
        except Exception as e:
            self.result.error(f"Failed to read file: {e}")
            return self.result
        
        return self.validate_string(raw, content_type, filepath)
    
    def validate_string(self, raw_json: str, content_type: Optional[ContentType] = None, 
                        filename: str = "<string>") -> ValidationResult:
        """验证 JSON 字符串"""
        if not self.result.messages:  # 不清空已有结果（支持批量）
            self.result = ValidationResult()
        
        # ContentParser: json.replace("#", "\\#")
        # Mindustry's Jval parser handles `\#`; Python's json module doesn't.
        # Strip bare # characters outside strings (used as comments by some modders)
        cleaned = self._strip_bare_hash(raw_json)
        
        # Strip single-line comments (// ...) and C-style comments (/* ... */)
        # Mindustry's Jval parser handles these; Python's json module doesn't
        cleaned = self._strip_json_comments(cleaned)
        
        # Strip trailing commas (Mindustry's parser tolerates them)
        cleaned = self._strip_trailing_commas(cleaned)
        
        # 1. JSON 语法检查
        try:
            data = json.loads(cleaned)
        except json.JSONDecodeError as e:
            self.result.error(f"JSON parse error: {e}", filename)
            return self.result
        
        # 如果 data 不是对象，直接报错
        if not isinstance(data, dict):
            self.result.error(f"Content must be a JSON object, got {type(data).__name__}", filename)
            return self.result
        
        # 如果没有指定类型，尝试从路径检测
        if content_type is None:
            content_type = self._detect_type_from_path(filename)
        
        # 2. 内容名称验证
        name = data.get("name", None)
        if name is None:
            # 从文件名推断
            base = os.path.splitext(os.path.basename(filename))[0]
            if base and base != "<string>":
                name = base
                self.result.warn(f"Missing 'name' field, inferred '{name}' from filename", filename)
        
        if name and " " in name:
            self.result.error(f"Content name '{name}' contains spaces. Use hyphens (-) instead.", filename)
        
        if name and name != name.lower():
            self.result.warn(f"Content name '{name}' should be lowercase", filename)
        
        # 3. 根据类型分派验证
        if content_type == ContentType.block:
            self._validate_block(data, name or "unknown", filename)
        elif content_type == ContentType.unit:
            self._validate_unit(data, name or "unknown", filename)
        elif content_type == ContentType.item:
            self._validate_item(data, name or "unknown", filename)
        elif content_type == ContentType.liquid:
            self._validate_liquid(data, name or "unknown", filename)
        elif content_type == ContentType.status:
            self._validate_status(data, name or "unknown", filename)
        elif content_type == ContentType.planet:
            self._validate_planet(data, name or "unknown", filename)
        elif content_type == ContentType.sector:
            self._validate_sector(data, name or "unknown", filename)
        elif content_type == ContentType.weather:
            self._validate_weather(data, name or "unknown", filename)
        elif content_type == ContentType.team:
            self._validate_team(data, name or "unknown", filename)
        
        # 4. 通用: 检查 research 字段
        if "research" in data:
            self._validate_research(data["research"], name or "unknown", filename)
        
        return self.result
    
    @staticmethod
    def _strip_bare_hash(text: str) -> str:
        """Remove bare # characters outside of strings (Mindustry treats them as comments)."""
        result = []
        i = 0
        in_string = False
        escape = False
        length = len(text)
        
        while i < length:
            ch = text[i]
            
            if escape:
                result.append(ch)
                escape = False
                i += 1
                continue
            
            if ch == '\\' and in_string:
                result.append(ch)
                escape = True
                i += 1
                continue
            
            if ch == '"' and not escape:
                in_string = not in_string
                result.append(ch)
                i += 1
                continue
            
            if in_string:
                result.append(ch)
                i += 1
                continue
            
            # Outside string: skip bare #
            if ch == '#':
                i += 1
                continue
            
            result.append(ch)
            i += 1
        
        return ''.join(result)
    
    # --- JSON comment stripping (matching Mindustry's tolerance) ---
    @staticmethod
    def _strip_json_comments(text: str) -> str:
        """Remove // and /* */ comments from JSON text, respecting string literals."""
        result = []
        i = 0
        in_string = False
        escape = False
        length = len(text)
        
        while i < length:
            ch = text[i]
            
            if escape:
                result.append(ch)
                escape = False
                i += 1
                continue
            
            if ch == '\\' and in_string:
                result.append(ch)
                escape = True
                i += 1
                continue
            
            if ch == '"' and not escape:
                in_string = not in_string
                result.append(ch)
                i += 1
                continue
            
            if in_string:
                result.append(ch)
                i += 1
                continue
            
            # Check for single-line comment
            if ch == '/' and i + 1 < length:
                next_ch = text[i + 1]
                if next_ch == '/':
                    # Skip to end of line
                    while i < length and text[i] != '\n':
                        i += 1
                    continue
                elif next_ch == '*':
                    # Skip to closing */
                    i += 2
                    while i + 1 < length:
                        if text[i] == '*' and text[i + 1] == '/':
                            i += 2
                            break
                        i += 1
                    continue
            
            result.append(ch)
            i += 1
        
        return ''.join(result)
    
    @staticmethod
    def _strip_trailing_commas(text: str) -> str:
        """Remove trailing commas before } or ] (Mindustry tolerates these)."""
        import re
        # Remove commas that are immediately followed by optional whitespace and then } or ]
        return re.sub(r',\s*([}\]])', r'\1', text)
    
    # --- Content type detection ---
    def _detect_type_from_path(self, filepath: str) -> Optional[ContentType]:
        """从文件路径检测内容类型"""
        p = filepath.lower().replace("\\", "/")
        if "/blocks/" in p or filepath.endswith(".json") and "block" in p:
            return ContentType.block
        if "/units/" in p or "/unit/" in p:
            return ContentType.unit
        if "/items/" in p:
            return ContentType.item
        if "/liquids/" in p:
            return ContentType.liquid
        if "/status/" in p or "/statuses/" in p:
            return ContentType.status
        if "/planets/" in p:
            return ContentType.planet
        if "/sectors/" in p or "/sector-presets/" in p:
            return ContentType.sector
        if "/weather/" in p or "/weathers/" in p:
            return ContentType.weather
        if "/teams/" in p:
            return ContentType.team
        return None
    
    # --- Block validation ---
    def _validate_block(self, data: dict, name: str, filename: str):
        """ContentParser block parser logic"""
        full_name = f"{self.mod_name}-{name}"
        
        # 检查 type 是否能解析
        type_str = data.get("type", "")
        if type_str:
            resolved = self._resolve_class(type_str)
            if resolved is None and type_str not in ("Block", ""):
                self.result.warn(
                    f"Block '{full_name}': No type '{type_str}' found, defaulting to type 'Block'",
                    filename
                )
            # 检查 type 值必须是字符串
            if not isinstance(type_str, str):
                self.result.error(
                    f"Block '{full_name}': 'type' must be a string, got {type(type_str).__name__}",
                    filename
                )
        
        # 检查已知字段
        self._warn_unknown_fields(data, BLOCK_FIELDS | TURRET_FIELDS, f"Block '{full_name}'", filename)
        
        # 检查 size
        size = data.get("size")
        if isinstance(size, int) and size > 6:
            self.result.error(
                f"Blocks cannot be larger than 6, got size={size}",
                filename
            )
        
        # 检查 buildVisibility 和 requirements
        if "requirements" in data and "buildVisibility" in data:
            bv = data["buildVisibility"]
            if bv == "hidden":
                self.result.warn(
                    f"Block '{full_name}': has requirements but buildVisibility is 'hidden'. "
                    f"ContentParser will auto-set to 'shown'.",
                    filename
                )
        
        # 检查 consumes 结构
        if "consumes" in data:
            self._validate_consumers(data["consumes"], full_name, filename)
        
        # 检查 drawer/draw 字段
        if "drawer" in data:
            self._validate_draw_block(data["drawer"], f"{full_name}.drawer", filename)
        if "draw" in data:
            self._validate_draw_block(data["draw"], f"{full_name}.draw", filename)
        
        # 检查 outputItems / outputLiquids
        if "outputItems" in data:
            self._validate_item_stacks(data["outputItems"], f"{full_name}.outputItems", filename)
        if "outputLiquids" in data:
            self._validate_liquid_stacks(data["outputLiquids"], f"{full_name}.outputLiquids", filename)
    
    # --- Unit validation ---
    def _validate_unit(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        type_str = data.get("type", "")
        if type_str and isinstance(type_str, str):
            if type_str not in ALLOWED_UNIT_TYPES:
                self.result.error(
                    f"Unit '{full_name}': Invalid unit type '{type_str}'. "
                    f"Must be one of: {', '.join(sorted(ALLOWED_UNIT_TYPES))}",
                    filename
                )
        elif type_str and not isinstance(type_str, str):
            self.result.error(
                f"Unit '{full_name}': 'type' must be a string, got {type(type_str).__name__}. "
                f"Unit types must be strings.",
                filename
            )
        
        template = data.get("template", "")
        if template and isinstance(template, str):
            if self._resolve_class(template) is None:
                self.result.warn(
                    f"Unit '{full_name}': template '{template}' not found in ClassMap",
                    filename
                )
        
        # controller / aiController
        controller = data.get("controller") or data.get("aiController", "")
        if controller:
            resolved = self._resolve_class(controller) if isinstance(controller, str) else None
            if resolved is None and isinstance(controller, str):
                self.result.warn(
                    f"Unit '{full_name}': AI controller '{controller}' not found",
                    filename
                )
        
        self._warn_unknown_fields(data, UNIT_TYPE_FIELDS, f"Unit '{full_name}'", filename)
        
        weapons = data.get("weapons")
        if isinstance(weapons, list):
            for i, w in enumerate(weapons):
                if isinstance(w, dict):
                    self._validate_weapon(w, f"{full_name}.weapons[{i}]", filename)
        
        abilities = data.get("abilities")
        if isinstance(abilities, list):
            for i, a in enumerate(abilities):
                if isinstance(a, dict):
                    self._validate_ability(a, f"{full_name}.abilities[{i}]", filename)
        
        if "requirements" in data:
            req = data["requirements"]
            if isinstance(req, dict):
                if "block" not in req:
                    self.result.error(
                        f"Unit '{full_name}': 'requirements' must have a 'block' field",
                        filename
                    )
    
    # --- Weapon validation ---
    def _validate_weapon(self, data: dict, path: str, filename: str):
        self._warn_unknown_fields(data, WEAPON_FIELDS, f"Weapon", f"{path} ({filename})")
        
        bullet = data.get("bullet")
        if isinstance(bullet, str):
            # 引用 bullet name
            pass
        elif isinstance(bullet, dict):
            self._validate_bullet_type(bullet, f"{path}.bullet", filename)
    
    # --- Bullet type validation ---
    def _validate_bullet_type(self, data: dict, path: str, filename: str):
        type_str = data.get("type", "")
        if type_str:
            resolved = self._resolve_class(f"{type_str}BulletType") or self._resolve_class(type_str)
            if resolved is None:
                self.result.warn(
                    f"Bullet type '{type_str}' not found in ClassMap",
                    f"{path} ({filename})"
                )
        
        # Check effect references
        for effect_field in ("hitEffect", "despawnEffect", "shootEffect", "smokeEffect"):
            if effect_field in data:
                self._validate_effect_ref(data[effect_field], f"{path}.{effect_field}", filename)
        
        # Check status references
        status = data.get("status")
        if isinstance(status, str) and status != "none":
            if status not in VANILLA_STATUS_EFFECTS and f"{self.mod_name}-{status}" not in self.custom_content:
                # status 可能是 mod 自定义的，只给 warn
                self.result.warn(
                    f"Status effect '{status}' not found in vanilla content (may be custom)",
                    f"{path} ({filename})"
                )
        
        # Check frag bullet reference
        frag = data.get("fragBullet")
        if isinstance(frag, str):
            pass  # string reference
        elif isinstance(frag, dict):
            self._validate_bullet_type(frag, f"{path}.fragBullet", filename)
        
        self._warn_unknown_fields(data, BULLET_TYPE_FIELDS, "BulletType", f"{path} ({filename})")
    
    # --- Ability validation ---
    def _validate_ability(self, data: dict, path: str, filename: str):
        type_str = data.get("type", "")
        if type_str:
            resolved = self._resolve_class(type_str)
            if resolved is None:
                self.result.error(
                    f"Unknown ability type: '{type_str}'",
                    f"{path} ({filename})"
                )
        else:
            self.result.error(
                f"Ability must have a 'type' field",
                f"{path} ({filename})"
            )
    
    # --- DrawBlock validation ---
    def _validate_draw_block(self, data: Any, path: str, filename: str):
        """Mirror ContentParser's DrawBlock parser"""
        if isinstance(data, str):
            # string shorthand: draw type by name
            resolved = self._resolve_class(data)
            if resolved is None and data not in DRAWBLOCK_TYPES:
                self.result.warn(
                    f"DrawBlock type '{data}' not found",
                    f"{path} ({filename})"
                )
            return
        
        if isinstance(data, list):
            # array shorthand → DrawMulti
            for i, item in enumerate(data):
                if isinstance(item, dict):
                    self._validate_draw_block(item, f"{path}[{i}]", filename)
                elif isinstance(item, str):
                    self._validate_draw_block(item, f"{path}[{i}]", filename)
            return
        
        if isinstance(data, dict):
            type_str = data.get("type", "")
            if type_str:
                resolved = self._resolve_class(type_str)
                if resolved is None:
                    self.result.warn(
                        f"DrawBlock type '{type_str}' not found, defaulting to DrawDefault",
                        f"{path} ({filename})"
                    )
    
    # --- Item validation ---
    def _validate_item(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        self._warn_unknown_fields(data, {
            "type", "name", "description", "localizedName",
            "color", "hardness", "radioactivity", "explosiveness",
            "flammability", "charge", "cost", "lowPriority",
            "boilPoint", "gas", "hidden", "bridgeIcon",
            "healthScaling",
        }, f"Item '{full_name}'", filename)
        
        color = data.get("color")
        if color:
            self._validate_color(color, f"{full_name}.color", filename)
    
    # --- Liquid validation ---
    def _validate_liquid(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        self._warn_unknown_fields(data, {
            "type", "name", "description", "localizedName",
            "color", "density", "viscosity", "temperature",
            "heatCapacity", "temperature", "lightColor",
            "explosiveness", "flammability", "radioactivity",
            "viscosity", "cost", "hidden", "gas",
            "boilPoint", "lowPressure", "highPressure",
        }, f"Liquid '{full_name}'", filename)
        
        color = data.get("color")
        if color:
            self._validate_color(color, f"{full_name}.color", filename)
    
    # --- Status effect validation ---
    def _validate_status(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        color = data.get("color")
        if color:
            self._validate_color(color, f"{full_name}.color", filename)
        
        # 检查 opposites 和 affinities
        for ref_field in ("opposites", "affinities"):
            if ref_field in data:
                refs = data[ref_field]
                if isinstance(refs, str):
                    refs = [refs]
                if isinstance(refs, list):
                    for r in refs:
                        if isinstance(r, str):
                            if r not in VANILLA_STATUS_EFFECTS and f"{self.mod_name}-{r}" not in self.custom_content:
                                self.result.warn(
                                    f"StatusEffect '{full_name}': {ref_field} references unknown effect '{r}'",
                                    filename
                                )
    
    # --- Planet validation ---
    def _validate_planet(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        # 检查 parent
        parent = data.get("parent", "")
        if parent:
            if isinstance(parent, str):
                if parent not in VANILLA_PLANETS and f"{self.mod_name}-{parent}" not in self.custom_content:
                    self.result.warn(
                        f"Planet '{full_name}': parent planet '{parent}' not found in vanilla",
                        filename
                    )
            elif parent is not None:
                self.result.error(
                    f"Planet '{full_name}': 'parent' must be a string or null",
                    filename
                )
        
        # 检查 mesh
        mesh = data.get("mesh")
        if mesh:
            if isinstance(mesh, dict):
                mesh_type = mesh.get("type", "NoiseMesh")
                known_mesh_types = {"NoiseMesh", "SunMesh", "HexSkyMesh", "MultiMesh", "MatMesh"}
                if mesh_type not in known_mesh_types:
                    self.result.warn(
                        f"Planet '{full_name}': unknown mesh type '{mesh_type}'",
                        filename
                    )
            elif isinstance(mesh, list):
                for i, m in enumerate(mesh):
                    if isinstance(m, dict):
                        mesh_type = m.get("type", "NoiseMesh")
                        known_mesh_types = {"NoiseMesh", "SunMesh", "HexSkyMesh", "MultiMesh", "MatMesh"}
                        if mesh_type not in known_mesh_types:
                            self.result.warn(
                                f"Planet '{full_name}': unknown mesh type '{mesh_type}' at index {i}",
                                filename
                            )
            else:
                self.result.error(
                    f"Planet '{full_name}': 'mesh' must be an object or array",
                    filename
                )
        
        # rules 必须是 object
        rules = data.get("rules")
        if rules and not isinstance(rules, dict):
            self.result.error(
                f"Planet '{full_name}': Rules must be an object!",
                filename
            )
    
    # --- Sector validation ---
    def _validate_sector(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        if "sector" not in data:
            self.result.error(
                f"SectorPreset '{full_name}': Must have a 'sector' number",
                filename
            )
        elif not isinstance(data.get("sector"), (int, float)):
            self.result.error(
                f"SectorPreset '{full_name}': 'sector' must be a number",
                filename
            )
        
        planet = data.get("planet")
        if planet and isinstance(planet, str):
            if planet not in VANILLA_PLANETS and f"{self.mod_name}-{planet}" not in self.custom_content:
                self.result.warn(
                    f"SectorPreset '{full_name}': Planet '{planet}' not found",
                    filename
                )
        
        rules = data.get("rules")
        if rules and not isinstance(rules, dict):
            self.result.error(
                f"SectorPreset '{full_name}': Rules must be an object!",
                filename
            )
    
    # --- Weather validation ---
    def _validate_weather(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        type_str = data.get("type")
        if type_str:
            resolved = self._resolve_class(type_str)
            if resolved is None:
                self.result.error(
                    f"Weather '{full_name}': Cannot resolve type '{type_str}'",
                    filename
                )
        else:
            self.result.warn(
                f"Weather '{full_name}': Missing 'type' field, defaulting to ParticleWeather",
                filename
            )
    
    # --- Team validation ---
    def _validate_team(self, data: dict, name: str, filename: str):
        full_name = f"{self.mod_name}-{name}"
        
        team = data.get("team")
        if team is None:
            self.result.error(
                f"Team '{full_name}': 'team' field missing",
                filename
            )
        elif isinstance(team, str):
            if team not in VANILLA_TEAMS and not any(t in team for t in ["blue", "sharded", "crux"]):
                self.result.error(
                    f"Unknown team: '{team}'",
                    filename
                )
        elif isinstance(team, int):
            if team < 0 or team > 20:
                self.result.error(
                    f"Unknown team: {team}. Team index out of range.",
                    filename
                )
        else:
            self.result.error(
                f"Unknown team: '{team}'. Team must either be a string or a number.",
                filename
            )
    
    # --- Consumer validation ---
    def _validate_consumers(self, consumes: Any, block_name: str, filename: str):
        """ContentParser.readBlockConsumers logic"""
        if isinstance(consumes, dict):
            for key, value in consumes.items():
                valid_consumer_keys = {
                    "remove", "item", "itemCharged", "itemFlammable",
                    "itemRadioactive", "itemExplosive", "itemList", "itemExplode",
                    "items", "itemsBoost",
                    "liquidFlammable", "liquid", "liquids", "coolant", "liquidsBoost",
                    "power", "powerBuffered",
                }
                if key not in valid_consumer_keys:
                    self.result.error(
                        f"Unknown consumption type: '{key}' for block '{block_name}'",
                        f"consumes ({filename})"
                    )
                
                # Validate power consumer
                if key == "power" and isinstance(value, (int, float)):
                    if value < 0:
                        self.result.warn(
                            f"Block '{block_name}': negative power consumption ({value})",
                            f"consumes.power ({filename})"
                        )
                
                # Validate item references
                if key in ("item", "itemCharged", "itemFlammable", "itemRadioactive", "itemExplosive", "itemExplode"):
                    if isinstance(value, str):
                        self._validate_content_ref(value, ContentType.item, f"consumes.{key}", filename)
                
                # Validate liquid references
                if key in ("liquid", "coolant", "liquidFlammable"):
                    if isinstance(value, dict):
                        liquid = value.get("liquid")
                        if liquid and isinstance(liquid, str):
                            self._validate_content_ref(liquid, ContentType.liquid, f"consumes.{key}.liquid", filename)
                    elif isinstance(value, str) and "/" in value:
                        liquid = value.split("/")[0]
                        self._validate_content_ref(liquid, ContentType.liquid, f"consumes.{key}", filename)
                
                # Validate items array
                if key in ("items", "itemsBoost"):
                    self._validate_item_stacks(value, f"consumes.{key}", filename)
                
                # Validate liquids array
                if key in ("liquids", "liquidsBoost"):
                    self._validate_liquid_stacks(value, f"consumes.{key}", filename)
        
        elif isinstance(consumes, list):
            self.result.warn(
                f"Block '{block_name}': 'consumes' should be an object, not an array",
                f"consumes ({filename})"
            )
    
    # --- Research validation ---
    def _validate_research(self, research: Any, content_name: str, filename: str):
        if isinstance(research, str):
            # Simple string parent reference
            pass  # 只是 parent 名称，无法完全验证（可能是同 mod 内容）
        elif isinstance(research, dict):
            parent = research.get("parent")
            if parent:
                if isinstance(parent, str):
                    pass  # parent reference
                else:
                    self.result.error(
                        f"'research.parent' must be a string, got {type(parent).__name__}",
                        f"{content_name}.research ({filename})"
                    )
            
            # Check objectives
            objectives = research.get("objectives")
            if objectives:
                if isinstance(objectives, list):
                    for i, obj in enumerate(objectives):
                        if isinstance(obj, dict):
                            type_str = obj.get("type", "")
                            if type_str:
                                resolved = self._resolve_class(type_str)
                                if resolved is None:
                                    self.result.warn(
                                        f"Unknown objective type: '{type_str}'",
                                        f"{content_name}.research.objectives[{i}] ({filename})"
                                    )
                else:
                    self.result.error(
                        f"'research.objectives' must be an array",
                        f"{content_name}.research ({filename})"
                    )
        elif research is not None:
            self.result.error(
                f"'research' must be a string or object",
                f"{content_name} ({filename})"
            )
    
    # --- Helper: Color validation ---
    def _validate_color(self, color_str: Any, path: str, filename: str):
        if not isinstance(color_str, str):
            # ContentParser: warn when colors are numbers, not strings
            if isinstance(color_str, (int, float)):
                num_str = str(int(color_str))
                if len(num_str) != 6 and len(num_str) != 8:
                    self.result.warn(
                        f"Colors should strings, not numbers. Make sure you have quotes around the "
                        f"value, or they will not be parsed correctly: '{color_str}'",
                        f"{path} ({filename})"
                    )
            else:
                self.result.error(
                    f"Color must be a string (hex), got {type(color_str).__name__}. "
                    f"Make sure you have quotes around the value.",
                    f"{path} ({filename})"
                )
            return
        
        clean = color_str.replace("#", "").replace("0x", "")
        if len(clean) != 6 and len(clean) != 8:
            self.result.warn(
                f"'{color_str}' doesn't look like a valid hex color. "
                f"Expected 6 or 8 hex characters.",
                f"{path} ({filename})"
            )
        else:
            try:
                int(clean, 16)
            except ValueError:
                self.result.error(
                    f"'{color_str}' is not a valid hex color",
                    f"{path} ({filename})"
                )
    
    # --- Helper: Effect reference validation ---
    def _validate_effect_ref(self, ref: Any, path: str, filename: str):
        if isinstance(ref, str):
            # Could be Fx reference or inline effect
            if ref not in KNOWN_FX and not ref.startswith(f"{self.mod_name}-"):
                self.result.warn(
                    f"Effect '{ref}' not found in known Fx fields",
                    f"{path} ({filename})"
                )
    
    # --- Helper: Item stack validation ---
    def _validate_item_stacks(self, data: Any, path: str, filename: str):
        if isinstance(data, list):
            for i, item in enumerate(data):
                if isinstance(item, str):
                    # "copper/3" format
                    if "/" in item:
                        parts = item.split("/")
                        item_name = parts[0]
                        self._validate_content_ref(item_name, ContentType.item, f"{path}[{i}]", filename)
                    else:
                        self._validate_content_ref(item, ContentType.item, f"{path}[{i}]", filename)
                elif isinstance(item, dict):
                    item_name = item.get("item")
                    if item_name and isinstance(item_name, str):
                        self._validate_content_ref(item_name, ContentType.item, f"{path}[{i}].item", filename)
        elif isinstance(data, str):
            if "/" in data:
                parts = data.split("/")
                self._validate_content_ref(parts[0], ContentType.item, path, filename)
    
    # --- Helper: Liquid stack validation ---
    def _validate_liquid_stacks(self, data: Any, path: str, filename: str):
        if isinstance(data, list):
            for i, item in enumerate(data):
                if isinstance(item, str):
                    if "/" in item:
                        parts = item.split("/")
                        self._validate_content_ref(parts[0], ContentType.liquid, f"{path}[{i}]", filename)
                    else:
                        self._validate_content_ref(item, ContentType.liquid, f"{path}[{i}]", filename)
                elif isinstance(item, dict):
                    liquid_name = item.get("liquid")
                    if liquid_name and isinstance(liquid_name, str):
                        self._validate_content_ref(liquid_name, ContentType.liquid, f"{path}[{i}].liquid", filename)
    
    # --- Helper: Content reference validation ---
    def _validate_content_ref(self, name: str, ctype: ContentType, path: str, filename: str):
        """Check if a content name exists in vanilla registry"""
        vanilla = {
            ContentType.item: VANILLA_ITEMS,
            ContentType.liquid: VANILLA_LIQUIDS,
            ContentType.block: VANILLA_BLOCKS,
            ContentType.unit: VANILLA_UNITS,
            ContentType.planet: VANILLA_PLANETS,
            ContentType.status: VANILLA_STATUS_EFFECTS,
            ContentType.weather: VANILLA_WEATHERS,
        }
        registry = vanilla.get(ctype, set())
        full_name = f"{self.mod_name}-{name}"
        
        if name not in registry and full_name not in self.custom_content:
            self.result.warn(
                f"No {ctype.value} found with name '{name}'. "
                f"Make sure '{name}' is spelled correctly, and that it really exists! "
                f"This may also occur because its file failed to parse.",
                f"{path} ({filename})"
            )
    
    # --- Helper: Class resolution ---
    def _resolve_class(self, name: str) -> Optional[str]:
        """ContentParser.resolve() logic"""
        if not name:
            return None
        
        # ClassMap lookup (capitalize if needed)
        if name and name[0].islower():
            name = name[0].upper() + name[1:]
        
        result = CLASS_MAP.get(name)
        if result:
            return result
        
        # Try direct package name
        if "." in name:
            return name  # Assume it's a full class name
        
        return None
    
    # --- Helper: Unknown field warning ---
    def _warn_unknown_fields(self, data: dict, known_fields: set, obj_name: str, filename: str):
        """ContentParser warns on unknown fields with ignoreUnknownFields = true"""
        for key in data.keys():
            if key not in known_fields and key not in {
                "name", "description", "localizedName", "research",
            }:
                self.result.warn(
                    f"Unknown field '{key}' for class '{obj_name}'",
                    filename
                )


# ============================================================
# 8. Batch validation
# ============================================================
def validate_directory(dirpath: str, mod_name: str = "mod", 
                       content_type: Optional[ContentType] = None) -> list[ValidationResult]:
    """验证整个 mod 目录"""
    results = []
    validator = ContentValidator(mod_name, content_type)
    
    for root, dirs, files in os.walk(dirpath):
        for f in files:
            if f.endswith(".json"):
                filepath = os.path.join(root, f)
                # 自动检测类型
                detected_type = content_type or validator._detect_type_from_path(filepath)
                v = ContentValidator(mod_name, detected_type)
                result = v.validate_file(filepath)
                if result.messages:
                    results.append(result)
                    # 收集已成功的内容名用于后续验证
                    for msg in result.messages:
                        pass  # TODO: collect parsed content names
    
    return results


# ============================================================
# 9. CLI interface
# ============================================================
def main():
    parser = argparse.ArgumentParser(
        description="Mindustry JSON Content Validator — mirrors ContentParser.java validation logic"
    )
    parser.add_argument("files", nargs="*", help="JSON files to validate")
    parser.add_argument("--type", "-t", choices=["block", "unit", "item", "liquid", "status", "planet", "sector", "weather", "team"],
                        help="Force content type (auto-detected from path if not specified)")
    parser.add_argument("--mod", "-m", default="mod", help="Mod name prefix (default: 'mod')")
    parser.add_argument("--dir", "-d", help="Validate all JSON files in directory")
    parser.add_argument("--json", "-j", action="store_true", help="Output as JSON")
    
    args = parser.parse_args()
    
    ct = ContentType(args.type) if args.type else None
    
    if args.dir:
        results = validate_directory(args.dir, args.mod, ct)
        all_msgs = []
        for r in results:
            all_msgs.extend(r.messages)
    else:
        if not args.files:
            parser.print_help()
            sys.exit(1)
        
        all_msgs = []
        for filepath in args.files:
            v = ContentValidator(args.mod, ct)
            result = v.validate_file(filepath)
            all_msgs.extend(result.messages)
    
    if args.json:
        output = [{"level": m.level, "message": m.message, "path": m.path} for m in all_msgs]
        print(json.dumps(output, indent=2, ensure_ascii=False))
    else:
        if not all_msgs:
            print("[OK] No errors or warnings found.")
        else:
            error_count = sum(1 for m in all_msgs if m.level == "ERROR")
            warn_count = sum(1 for m in all_msgs if m.level == "WARN")
            
            for msg in all_msgs:
                print(str(msg))
            
            print(f"\n--- Summary: {error_count} error(s), {warn_count} warning(s) ---")
    
    # Exit code: 1 if there are errors
    sys.exit(1 if any(m.level == "ERROR" for m in all_msgs) else 0)


if __name__ == "__main__":
    main()
