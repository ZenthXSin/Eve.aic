package modvalidator.core;

import arc.*;
import arc.graphics.g2d.*;
import arc.struct.*;
import mindustry.*;
import mindustry.ctype.*;
import mindustry.gen.*;
import mindustry.world.*;
import mindustry.entities.part.*;
import mindustry.world.draw.*;

import java.lang.reflect.*;
import java.util.*;

/**
 * Analyzes DrawBlock subclasses for rendering issues:
 * - Detects glow/additive textures that may be missing
 * - Checks for shader-related fields
 * - Reports drawer type and key fields per block
 */
public class RenderPipelineMonitor {

    private final List<RenderIssue> issues = new ArrayList<>();
    private final List<DrawBlockInfo> drawBlockInfos = new ArrayList<>();

    /** Analyze all mod blocks' DrawBlock configurations. */
    public void analyze(){
        for(Content c : Vars.content.getBy(ContentType.block)){
            if(c.minfo.mod == null) continue;
            Block block = (Block) c;
            analyzeBlock(block);
        }
    }

    private void analyzeBlock(Block block){
        DrawBlock drawer = getDrawer(block);
        if(drawer == null) return;

        String drawerType = drawer.getClass().getSimpleName();

        // Extract DrawBlock fields via reflection
        Map<String, Object> fields = extractDrawFields(drawer);
        DrawBlockInfo info = new DrawBlockInfo(block.name, drawerType, fields);

        // Check specific drawer types for common issues
        if(drawer instanceof DrawTurret turret){
            checkDrawTurret(block.name, turret, info);
        }
        if(drawer instanceof DrawFlame flame){
            checkDrawFlame(block.name, flame, info);
        }
        if(drawer instanceof DrawGlowRegion glow){
            checkDrawGlow(block.name, glow, info);
        }
        if(drawer instanceof DrawHeatOutput heat){
            checkDrawHeatOutput(block.name, heat, info);
        }
        if(drawer instanceof DrawFrames frames){
            checkDrawFrames(block.name, frames, info);
        }

        // Check if emitLight is set but drawer doesn't implement drawLight
        if(block.emitLight && !overridesDrawLight(drawer)){
            issues.add(new RenderIssue(block.name, RenderIssueType.EMIT_LIGHT_NO_DRAW,
                "emitLight=true 但 " + drawerType + " 未重写 drawLight()"));
        }

        drawBlockInfos.add(info);
    }

    private void checkDrawTurret(String blockName, DrawTurret turret, DrawBlockInfo info){
        // Check for missing base texture
        try{
            Field baseField = DrawTurret.class.getDeclaredField("base");
            baseField.setAccessible(true);
            TextureRegion base = (TextureRegion) baseField.get(turret);
            if(base == null || !baseFound(base)){
                issues.add(new RenderIssue(blockName, RenderIssueType.MISSING_TEXTURE,
                    "DrawTurret base 贴图缺失"));
            }
        }catch(Exception ignored){}

        // Check parts
        try{
            Field partsField = DrawTurret.class.getDeclaredField("parts");
            partsField.setAccessible(true);
            @SuppressWarnings("unchecked")
            Seq<DrawPart> parts = (Seq<DrawPart>) partsField.get(turret);
            if(parts != null && parts.isEmpty()){
                issues.add(new RenderIssue(blockName, RenderIssueType.EMPTY_PARTS,
                    "DrawTurret.parts 为空"));
            }
        }catch(Exception ignored){}
    }

    private void checkDrawFlame(String blockName, DrawFlame flame, DrawBlockInfo info){
        try{
            Field topField = DrawFlame.class.getDeclaredField("top");
            topField.setAccessible(true);
            TextureRegion top = (TextureRegion) topField.get(flame);
            if(top == null || !topFound(top)){
                issues.add(new RenderIssue(blockName, RenderIssueType.MISSING_TEXTURE,
                    "DrawFlame top 贴图缺失 (load() 中硬编码 Core.atlas.find(name + \"-top\"))"));
            }
        }catch(Exception ignored){}
    }

    private void checkDrawGlow(String blockName, DrawGlowRegion glow, DrawBlockInfo info){
        try{
            Field glowField = DrawGlowRegion.class.getDeclaredField("glow");
            glowField.setAccessible(true);
            TextureRegion g = (TextureRegion) glowField.get(glow);
            if(g == null || !g.found()){
                issues.add(new RenderIssue(blockName, RenderIssueType.MISSING_TEXTURE,
                    "DrawGlowRegion glow 贴图缺失"));
            }
        }catch(Exception ignored){}
    }

    private void checkDrawHeatOutput(String blockName, DrawHeatOutput heat, DrawBlockInfo info){
        try{
            Field glowField = DrawHeatOutput.class.getDeclaredField("glow");
            glowField.setAccessible(true);
            TextureRegion g = (TextureRegion) glowField.get(heat);
            if(g != null && !g.found()){
                issues.add(new RenderIssue(blockName, RenderIssueType.OPTIONAL_MISSING,
                    "DrawHeatOutput glow 贴图缺失（可选）"));
            }
        }catch(Exception ignored){}
    }

    private void checkDrawFrames(String blockName, DrawFrames frames, DrawBlockInfo info){
        try{
            Field regionsField = DrawFrames.class.getDeclaredField("regions");
            regionsField.setAccessible(true);
            TextureRegion[] regions = (TextureRegion[]) regionsField.get(frames);
            if(regions != null){
                int missing = 0;
                for(TextureRegion r : regions){
                    if(r == null || !r.found()) missing++;
                }
                if(missing > 0){
                    issues.add(new RenderIssue(blockName, RenderIssueType.PARTIAL_FRAMES,
                        "DrawFrames: " + missing + "/" + regions.length + " 帧贴图缺失"));
                }
            }
        }catch(Exception ignored){}
    }

    @SuppressWarnings("unchecked")
    private static DrawBlock getDrawer(Block block){
        try{
            Field f = findDrawerField(block.getClass());
            if(f != null){
                f.setAccessible(true);
                return (DrawBlock) f.get(block);
            }
        }catch(Exception ignored){}
        return null;
    }

    private static Field findDrawerField(Class<?> clazz){
        while(clazz != null && clazz != Object.class){
            try{
                return clazz.getDeclaredField("drawer");
            }catch(NoSuchFieldException e){
                clazz = clazz.getSuperclass();
            }
        }
        return null;
    }

    private boolean overridesDrawLight(DrawBlock drawer){
        try{
            // Check if the drawer class overrides drawLight
            Method m = drawer.getClass().getDeclaredMethod("drawLight", Building.class);
            return m.getDeclaringClass() != DrawBlock.class;
        }catch(NoSuchMethodException e){
            return false;
        }
    }

    private boolean baseFound(TextureRegion region){
        return region != null && region.found();
    }

    private boolean topFound(TextureRegion region){
        return region != null && region.found();
    }

    private Map<String, Object> extractDrawFields(DrawBlock drawer){
        Map<String, Object> result = new LinkedHashMap<>();
        Class<?> clazz = drawer.getClass();
        while(clazz != null && clazz != Object.class && clazz != DrawBlock.class){
            for(Field f : clazz.getDeclaredFields()){
                if(Modifier.isStatic(f.getModifiers())) continue;
                try{
                    f.setAccessible(true);
                    Object val = f.get(drawer);
                    if(val instanceof TextureRegion tr){
                        result.put(f.getName(), tr.found() ? "found" : "NOT_FOUND");
                    }else if(val != null && isSimpleValue(val)){
                        result.put(f.getName(), val);
                    }
                }catch(Exception ignored){}
            }
            clazz = clazz.getSuperclass();
        }
        return result;
    }

    private boolean isSimpleValue(Object val){
        return val instanceof Number || val instanceof String || val instanceof Boolean ||
               val instanceof Enum || val instanceof Class;
    }

    public List<RenderIssue> getIssues(){ return issues; }
    public List<DrawBlockInfo> getDrawBlockInfos(){ return drawBlockInfos; }

    public record DrawBlockInfo(String blockName, String drawerType, Map<String, Object> fields){}
    public record RenderIssue(String blockName, RenderIssueType type, String detail){}

    public enum RenderIssueType {
        MISSING_TEXTURE, OPTIONAL_MISSING, EMPTY_PARTS, EMIT_LIGHT_NO_DRAW, PARTIAL_FRAMES
    }
}
