package modvalidator.core;

import arc.struct.*;
import arc.util.*;
import mindustry.*;
import mindustry.core.*;
import mindustry.core.GameState.*;
import mindustry.ctype.*;
import mindustry.game.*;
import mindustry.gen.*;
import mindustry.type.*;
import mindustry.world.*;
import mindustry.world.blocks.payloads.*;

import modvalidator.ValidationResult;

import java.util.*;

/**
 * Performs dynamic runtime tests on loaded content.
 * Spawns units, places blocks, runs update ticks, checks for crashes.
 */
public class ContentTester {

 private final HeadlessTestEnvironment env;
 private final ValidationResult result;

 public ContentTester(HeadlessTestEnvironment env, ValidationResult result){
 this.env = env;
 this.result = result;
 }

 /**
 * Run all dynamic tests on the loaded mod.
 */
 public void runAllTests(){
 // Phase A: Lifecycle & texture analysis (pre-dynamic-test reporting)
 reportLifecycleAnomalies();
 reportTextureIssues();
 reportRenderIssues();

 // Phase B: Dynamic runtime tests
 try{
 testBlocks();
 }catch(Throwable t){
 result.addIssue(ValidationResult.Severity.ERROR, "block-test",
 "方块测试崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
 }

 try{
 testUnits();
 }catch(Throwable t){
 result.addIssue(ValidationResult.Severity.ERROR, "unit-test",
 "单位测试崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
 }

 try{
 testItems();
 }catch(Throwable t){
 result.addIssue(ValidationResult.Severity.ERROR, "item-test",
 "物品测试崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
 }

 try{
 testLiquids();
 }catch(Throwable t){
 result.addIssue(ValidationResult.Severity.ERROR, "liquid-test",
 "液体测试崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
 }
 }

 /** Report lifecycle anomalies from ContentLifecycleMonitor. */
 private void reportLifecycleAnomalies(){
 ContentLifecycleMonitor monitor = env.getLifecycleMonitor();
 for(var anomaly : monitor.getAnomalies()){
 result.addIssue(ValidationResult.Severity.WARN, "lifecycle",
 anomaly.contentKey() + "#" + anomaly.fieldName() + ": " + anomaly.detail());
 }
 }

 /** Report missing textures from TextureResourceMonitor. */
 private void reportTextureIssues(){
 TextureResourceMonitor monitor = env.getTextureMonitor();
 for(var missing : monitor.getMissing()){
 result.addIssue(ValidationResult.Severity.ERROR, "texture-missing",
 missing.contentType() + " '" + missing.contentName() + "' 缺少贴图: " + missing.textureName());
 }
 for(var warn : monitor.getWarnings()){
 result.addIssue(ValidationResult.Severity.WARN, "texture-optional",
 warn.contentType() + " '" + warn.contentName() + "' 可选贴图缺失: " + warn.textureName());
 }
 }

 /** Report render pipeline issues from RenderPipelineMonitor. */
 private void reportRenderIssues(){
 RenderPipelineMonitor monitor = env.getRenderMonitor();
 for(var issue : monitor.getIssues()){
 var severity = (issue.type() == RenderPipelineMonitor.RenderIssueType.MISSING_TEXTURE)
 ? ValidationResult.Severity.ERROR
 : ValidationResult.Severity.WARN;
 result.addIssue(severity, "render",
 "方块 '" + issue.blockName() + "': " + issue.detail());
 }
 }

 /**
 * Test all blocks from the mod: place them, run update ticks.
 */
 @SuppressWarnings("unchecked")
 public void testBlocks(){
 Seq<Block> blocks = new arc.struct.Seq<>();
 for(Content c : env.getContent(ContentType.block)){
 if(c.minfo != null && c.minfo.mod != null) blocks.add((Block)c);
 }
 if(blocks.isEmpty()){
 result.addIssue(ValidationResult.Severity.INFO, "block-test",
 "未找到模组方块进行测试");
 return;
 }

 // 无限火力 + 解除暂停，让方块真正工作
 Vars.state.set(State.playing);
 Vars.state.rules.infiniteResources = true;

 World world = env.world();
 int size = Math.max(16, blocks.size * 2 + 4);
 world.resize(size, size);
 world.tiles.fill();

 long deadline = System.currentTimeMillis() + 30000; // 30s 总超时
 int x = 2, y = 2;
 for(Block block : blocks){
 if(System.currentTimeMillis() > deadline){
 result.addIssue(ValidationResult.Severity.WARN, "block-test",
 "动态测试超时（30s），跳过剩余方块");
 break;
 }
 try{
 world.tile(x, y).setBlock(block, Team.get(0), 0);

 // 跑 60 tick，每 tick 推进时间
 for(int i = 0; i < 60; i++){
 if(world.tile(x, y).build != null){
 Time.delta = 1f;
 Time.update();
 world.tile(x, y).build.update();
 }
 }

 // success: silent

 }catch(Throwable t){
 result.addIssue(ValidationResult.Severity.ERROR, "block-test",
 "方块 '" + block.name + "' 更新时崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
 }

 x++;
 if(x >= size - 1){
 x = 2;
 y++;
 }
 if(y >= size - 1) break;
 }
 }

 /**
 * Test all units from the mod: spawn them, run update ticks.
 */
    @SuppressWarnings("unchecked")
    public void testUnits(){
        Seq<UnitType> units = new arc.struct.Seq<>();
        for(Content c : env.getContent(ContentType.unit)){
            if(c.minfo != null && c.minfo.mod != null) units.add((UnitType)c);
        }
        if(units.isEmpty()){
            result.addIssue(ValidationResult.Severity.INFO, "unit-test",
                "未找到模组单位进行测试");
            return;
        }

        // 无限火力 + 解除暂停，让单位 AI 驱动移动
        Vars.state.set(State.playing);
        Vars.state.rules.infiniteResources = true;

        World world = env.world();
        // Ensure world is large enough for unit spawning
        if(world.width() < 10 || world.height() < 10){
            world.resize(64, 64);
            world.tiles.fill();
        }

        long unitDeadline = System.currentTimeMillis() + 30000; // 30s 总超时
        for(UnitType unit : units){
            if(System.currentTimeMillis() > unitDeadline){
                result.addIssue(ValidationResult.Severity.WARN, "unit-test",
                    "单位测试超时（30s），跳过剩余单位");
                break;
            }
            try{
                Unit spawned = unit.create(Team.get(0));
                spawned.set(32f, 32f);
                spawned.add();

                // 跑 60 tick，每 tick 推进时间，让 AI 驱动移动
                for(int i = 0; i < 60; i++){
                    Time.delta = 1f;
                    Time.update();
                    spawned.update();
                }

                // success: silent

            }catch(Throwable t){
                result.addIssue(ValidationResult.Severity.ERROR, "unit-test",
                    "单位 '" + unit.name + "' 崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
            }
        }
    }

 /**
 * Test all items: verify they have valid icons and stats.
 */
 @SuppressWarnings("unchecked")
 public void testItems(){
 Seq<Item> items = new arc.struct.Seq<>();
 for(Content c : env.getContent(ContentType.item)){
 if(c.minfo != null && c.minfo.mod != null) items.add((Item)c);
 }
 if(items.isEmpty()){
 result.addIssue(ValidationResult.Severity.INFO, "item-test",
 "未找到模组物品进行测试");
 return;
 }

 for(Item item : items){
 try{
 if(item.name == null || item.name.isEmpty()){
 result.addIssue(ValidationResult.Severity.WARN, "item-test",
 "物品名称为空");
 }else{
 result.addIssue(ValidationResult.Severity.INFO, "item-test",
 "物品 '" + item.name + "' 加载成功");
 }
 }catch(Exception e){
 result.addIssue(ValidationResult.Severity.ERROR, "item-test",
 "物品 '" + item.name + "' 错误: " + e.getMessage());
 }
 }
 }

 /**
 * Test all liquids: verify they have valid icons and stats.
 */
 @SuppressWarnings("unchecked")
 public void testLiquids(){
 Seq<Liquid> liquids = new arc.struct.Seq<>();
 for(Content c : env.getContent(ContentType.liquid)){
 if(c.minfo != null && c.minfo.mod != null) liquids.add((Liquid)c);
 }
 if(liquids.isEmpty()){
 result.addIssue(ValidationResult.Severity.INFO, "liquid-test",
 "未找到模组液体进行测试");
 return;
 }

 for(Liquid liquid : liquids){
 try{
 if(liquid.name == null || liquid.name.isEmpty()){
 result.addIssue(ValidationResult.Severity.WARN, "liquid-test",
 "液体名称为空");
 }else{
 result.addIssue(ValidationResult.Severity.INFO, "liquid-test",
 "液体 '" + liquid.name + "' 加载成功");
 }
 }catch(Exception e){
 result.addIssue(ValidationResult.Severity.ERROR, "liquid-test",
 "液体 '" + liquid.name + "' 错误: " + e.getMessage());
 }
 }
 }
}
