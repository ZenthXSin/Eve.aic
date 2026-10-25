package modvalidator.core;

import arc.*;
import arc.graphics.g2d.*;
import arc.struct.*;
import mindustry.*;
import mindustry.ctype.*;
import mindustry.mod.Mods.*;
import mindustry.type.*;
import mindustry.world.*;
import mindustry.world.draw.*;

import java.util.*;

/**
 * Detects missing texture regions for mod content.
 * Checks base region, -top, -glow, -preview, -outline, -liquid, -heat, -base, -team-* suffixes.
 */
public class TextureResourceMonitor {

    private final List<MissingTexture> missing = new ArrayList<>();
    private final List<MissingTexture> warnings = new ArrayList<>();

    /** Run full texture check on all mod content. */
    public void checkAll(){
        checkBlocks();
        checkUnits();
        checkItems();
        checkLiquids();
        checkBullets();
    }

    public void checkBlocks(){
        for(Content c : Vars.content.getBy(ContentType.block)){
            if(c.minfo.mod == null) continue;
            Block block = (Block) c;
            String name = block.name;

            // Base region
            checkRequired("block", name, name);

            // Common suffixes for blocks
            checkOptional("block", name, name + "-team-sharded");
            checkOptional("block", name, name + "-team-derelict");

            // If block has a DrawBlock (via reflection — drawer is on subclasses like GenericCrafter/Turret)
            Object drawer = getDrawer(block);
            if(drawer instanceof DrawTurret){
                checkOptional("block", name, name + "-preview");
                checkOptional("block", name, name + "-outline");
                checkOptional("block", name, name + "-liquid");
                checkOptional("block", name, name + "-heat");
                checkOptional("block", name, name + "-base");
                checkOptional("block", name, name + "-top");
            }
            if(drawer instanceof DrawFlame){
                checkRequired("block", name, name + "-top");
            }
            if(drawer instanceof DrawWarmupRegion){
                checkRequired("block", name, name + "-top");
            }
            if(drawer instanceof DrawHeatOutput){
                checkOptional("block", name, name + "-glow");
            }
            if(drawer instanceof DrawFrames){
                for(int i = 0; i < 8; i++){
                    checkOptional("block", name, name + "-frame" + i);
                }
            }

            // Variant regions
            if(block.variants > 0){
                for(int i = 0; i < block.variants; i++){
                    checkRequired("block", name, name + (i + 1));
                }
                if(block.customShadow){
                    for(int i = 0; i < block.variants; i++){
                        checkOptional("block", name, name + "-shadow" + (i + 1));
                    }
                }
            }
        }
    }

    public void checkUnits(){
        for(Content c : Vars.content.getBy(ContentType.unit)){
            if(c.minfo.mod == null) continue;
            UnitType unit = (UnitType) c;
            String name = unit.name;

            checkRequired("unit", name, name);
            checkOptional("unit", name, name + "-preview");
            checkOptional("unit", name, name + "-outline");

            // Legged units
            if(unit.legCount > 0){
                checkOptional("unit", name, name + "-leg");
                checkOptional("unit", name, name + "-foot");
                checkOptional("unit", name, name + "-joint");
            }

            // Mech units
            if(unit.mechSideSway > 0 || unit.mechStride > 0){
                checkOptional("unit", name, name + "-base");
            }

            // Wreck regions
            if(unit.createScorch){
                for(int i = 0; i < 3; i++){
                    checkOptional("unit", name, name + "-wreck" + i);
                }
            }
        }
    }

    public void checkItems(){
        for(Content c : Vars.content.getBy(ContentType.item)){
            if(c.minfo.mod == null) continue;
            Item item = (Item) c;
            checkRequired("item", item.name, item.name);
        }
    }

    public void checkLiquids(){
        for(Content c : Vars.content.getBy(ContentType.liquid)){
            if(c.minfo.mod == null) continue;
            Liquid liquid = (Liquid) c;
            // Liquids use icon, not region — check via Core.atlas
            checkRequired("liquid", liquid.name, liquid.name);
        }
    }

    public void checkBullets(){
        // BulletType 没有 name 字段（不继承 MappableContent），
        // 其贴图由 BulletType.load() 内部通过 Core.atlas.find(name) 加载，
        // 名称来自 JSON key，无法通过反射直接获取，暂跳过贴图检查。
    }

    /** Get the DrawBlock drawer from a Block via reflection (field exists on subclasses). */
    private Object getDrawer(Block block){
        try{
            java.lang.reflect.Field f = findDrawerField(block.getClass());
            if(f != null){
                f.setAccessible(true);
                return f.get(block);
            }
        }catch(Exception ignored){}
        return null;
    }

    private java.lang.reflect.Field findDrawerField(Class<?> clazz){
        while(clazz != null && clazz != Object.class){
            try{
                return clazz.getDeclaredField("drawer");
            }catch(NoSuchFieldException e){
                clazz = clazz.getSuperclass();
            }
        }
        return null;
    }

    private void checkRequired(String contentType, String contentName, String textureName){
        if(Core.atlas == null) return; // headless 模式下 atlas 未初始化
        if(!Core.atlas.has(textureName)){
            missing.add(new MissingTexture(contentType, contentName, textureName, true));
        }
    }

    private void checkOptional(String contentType, String contentName, String textureName){
        if(Core.atlas == null) return; // headless 模式下 atlas 未初始化
        if(!Core.atlas.has(textureName)){
            warnings.add(new MissingTexture(contentType, contentName, textureName, false));
        }
    }

    public List<MissingTexture> getMissing(){ return missing; }
    public List<MissingTexture> getWarnings(){ return warnings; }
    public boolean hasMissing(){ return !missing.isEmpty(); }

    public record MissingTexture(String contentType, String contentName, String textureName, boolean required){}
}
