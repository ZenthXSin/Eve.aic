package modvalidator.core;

import arc.*;
import arc.backend.headless.*;
import arc.files.*;
import arc.files.ZipFi;
import arc.struct.*;
import arc.util.*;
import mindustry.*;
import mindustry.core.*;
import mindustry.core.Version;
import mindustry.ctype.*;
import mindustry.gen.*;
import mindustry.mod.*;
import mindustry.mod.Mods.*;
import mindustry.net.*;
import mindustry.ui.*;
import mindustry.maps.Maps;
import mindustry.world.*;

import java.util.*;
import java.util.concurrent.*;
import java.util.concurrent.atomic.*;

import static mindustry.Vars.*;

/**
 * Manages a headless Mindustry environment for dynamic mod testing.
 * Mirrors ServerLauncher.init() setup sequence exactly.
 */
public class HeadlessTestEnvironment {

    private final Fi testDataDir;
    private final AtomicBoolean initialized = new AtomicBoolean(false);
    private final AtomicReference<Throwable> initError = new AtomicReference<>();
    private final CopyOnWriteArrayList<String> errorLogs = new CopyOnWriteArrayList<>();
    private final CopyOnWriteArrayList<String> warnLogs = new CopyOnWriteArrayList<>();
    private final CopyOnWriteArrayList<String> allLogs = new CopyOnWriteArrayList<>();
    private LoadedMod importedMod = null;
    private final ContentLifecycleMonitor lifecycleMonitor = new ContentLifecycleMonitor();
    private final TextureResourceMonitor textureMonitor = new TextureResourceMonitor();
    private final RenderPipelineMonitor renderMonitor = new RenderPipelineMonitor();

    // The Mindustry major version this validator targets (matches build.gradle mindustryVersion)
private static final int VALIDATOR_MINDUSTRY_MAJOR = 159;

private static void debugLog(String msg){
        System.err.println(msg);
    }


    public HeadlessTestEnvironment(Fi testDataDir){
        this.testDataDir = testDataDir;
    }

    public HeadlessTestEnvironment(String testDataPath){
        this(new Fi(testDataPath));
    }

    /**
     * Initialize the headless Mindustry environment.
     * Mirrors ServerLauncher.init() sequence.
     *
     * @param modPath optional path to a mod file/directory to import before content creation
     */
    public synchronized void initialize(String modPath) throws Exception {
        if(initialized.get()) return;

        testDataDir.deleteDirectory();
        Log.useColors = false;

        Log.logger = new Log.LogHandler() {
            @Override
            public void log(Log.LogLevel level, String text){
                allLogs.add("[" + level + "] " + text);
                if(level == Log.LogLevel.err){
                    errorLogs.add(text);
                }else if(level == Log.LogLevel.warn){
                    warnLogs.add(text);
                }
            }
        };

        try{
            boolean[] begins = {false};

            ApplicationCore core = new ApplicationCore(){
                @Override
                public void setup(){
                    debugLog("[调试] 阶段1: setup() 开始");
                    // Phase 1: Core systems (matches ServerLauncher.init())
                    Core.settings.setDataDirectory(testDataDir);
                    loadLocales = false;
                    headless = true;

                    Vars.loadSettings();
                    Vars.init();
                    debugLog("[调试] 阶段1: Vars.init() 完成");

                    UI.loadColors();
                    Fonts.loadContentIconsHeadless();

                    // Phase 2: Base content (skip campaign/sector presets - not needed for mod validation)
                    mindustry.ai.UnitCommand.loadAll();
                    mindustry.content.TeamEntries.load();
                    mindustry.content.Items.load();
                    mindustry.ai.UnitStance.loadAll();
                    mindustry.content.StatusEffects.load();
                    mindustry.content.Liquids.load();
                    mindustry.content.Bullets.load();
                    mindustry.content.UnitTypes.load();
                    mindustry.content.Blocks.load();
                    mindustry.content.Loadouts.load();
                    mindustry.content.Weathers.load();
                    // Load vanilla planets/sectors/tech trees (needed for mod tech tree hooks like addToResearch)
                    // Set maps=null so FileMapGenerator skips map loading (headless has no jar assets)
                    Maps savedMaps = Vars.maps;
                    Vars.maps = null;
                    mindustry.content.Planets.load();
                    mindustry.content.SectorPresets.load();
                    mindustry.content.SerpuloTechTree.load();
                    mindustry.content.ErekirTechTree.load();
                    Vars.maps = savedMaps;
                    mods.loadScripts();

                    debugLog("[调试] 阶段3: 导入模组开始");
                    // Phase 3: Import mod BEFORE createModContent
                    if(modPath != null){
                        try{
                            Fi modFile = new Fi(modPath);
                            if(modFile.exists()){
                                Fi zipFile;
                                if(modFile.isDirectory()){
                                    zipFile = new Fi(modFile.path() + ".zip");
                                    zipDirectory(modFile, zipFile);
                                }else{
                                    zipFile = modFile;
                                }
                                importedMod = Vars.mods.importMod(zipFile);
                                debugLog("[调试] importMod 完成, importedMod=" + (importedMod != null ? importedMod.name : "null"));
                                if(importedMod == null){
                                    initError.compareAndSet(null, new RuntimeException("importMod 返回 null: " + modPath));
                                }else{
                                    // Phase 3.3: Check mod.json minGameVersion against validator's Mindustry version
                                    try{
                                        ModMeta meta = Vars.mods.findMeta(importedMod.file.isDirectory() ? importedMod.file : new ZipFi(importedMod.file));
                                        if(meta != null && meta.minGameVersion != null && !meta.minGameVersion.isEmpty() && !"0".equals(meta.minGameVersion)){
                                            int modMinMajor = meta.getMinMajor();
                                            if(modMinMajor > VALIDATOR_MINDUSTRY_MAJOR){
                                                Log.warn("[验证器] 版本不匹配: 模组要求 Mindustry >= " + meta.minGameVersion + " (主版本: " + modMinMajor + "), 但验证器仅支持主版本 " + VALIDATOR_MINDUSTRY_MAJOR + "，可能导致加载失败");
                                            }else{
                                                debugLog("[调试] 版本检查通过: 模组最低=" + meta.minGameVersion + ", 验证器主版本=" + VALIDATOR_MINDUSTRY_MAJOR);
                                            }
                                        }
                                    }catch(Throwable t){
                                        debugLog("[调试] 版本检查跳过: " + t.getMessage());
                                    }
                                }
                            }else{
                                initError.compareAndSet(null, new RuntimeException("模组未找到: " + modPath));
                            }
                        }catch(Throwable t){
                            initError.compareAndSet(null, new RuntimeException("导入模组失败: " + t.getMessage(), t));
                        }
                    }

                    // Phase 3.4: Register imported mod files to FileTree (buildFiles equivalent)
                    if(importedMod != null && importedMod.root != null){
                        try{
                            String parentName = importedMod.file.isDirectory() ? null : (importedMod.root.parent() != null ? importedMod.root.name() : null);
                            for(Fi file : importedMod.root.list()){
                                if(file.isDirectory() && !file.name().equals("bundles") && !file.name().equals("sprites") && !file.name().equals("sprites-override") && !file.name().equals(".git")){
                                    Seq<Fi> files = new Seq<>();
                                    file.walk(f -> files.add(f));
                                    for(Fi f : files){
                                        if(!f.isDirectory()){
                                            String path;
                                            if(importedMod.file.isDirectory()){
                                                path = f.path().substring(1 + importedMod.file.path().length());
                                            }else if(parentName != null){
                                                path = f.path().substring(parentName.length() + 1);
                                            }else{
                                                path = f.path();
                                            }
                                            Vars.tree.addFile(path, f);
                                        }
                                    }
                                }
                            }
                            debugLog("[调试] 阶段3.4: 已注册模组文件到文件树");
                        }catch(Throwable t){
                            debugLog("[调试] 阶段3.4: 注册模组文件错误: " + t);
                        }
                    }

                    debugLog("[调试] 阶段3.5: loadScripts 开始, 模组数=" + mods.list().size);
                    // Phase 3.5: Load mod JS scripts (must run after importMod, before createModContent)
                    try{
                        debugLog("[调试] loadScripts 之前");
                        mods.loadScripts();
                        debugLog("[调试] loadScripts 之后");
                        debugLog("[调试] 阶段3.5: loadScripts 完成");
                    }catch(Throwable t){
                        Log.err("[验证器] loadScripts 抛出异常: " + t);
                        Log.err(t);
                    }

                    debugLog("[调试] 阶段4: createModContent 开始");
                    // Phase 4: Create mod content (scans imported mods)
                    try{
                        content.createModContent();
                    }catch(Throwable t){
                        Log.err("[验证器] createModContent 崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
                        initError.compareAndSet(null, new RuntimeException("createModContent 阶段崩溃: " + t.getMessage(), t));
                    }
                    debugLog("[调试] 单位数量: " + content.getBy(ContentType.unit).size + ", 方块数量: " + content.getBy(ContentType.block).size);

                    if(initError.get() == null){
                        try{
                            int pc2 = content.getBy(ContentType.planet).size;
                            debugLog("[调试] 星球数量: " + pc2);
                            for(int i = 0; i < pc2; i++){
                                debugLog("[调试] 星球[" + i + "]: " + content.getBy(ContentType.planet).get(i).toString());
                            }
                        }catch(Throwable t){
                            debugLog("[调试] 列出星球错误: " + t);
                        }

                        // Phase 4.5: Capture post-create snapshots (before init)
                        try{
                            lifecycleMonitor.captureAll("post-create");
                        }catch(Throwable t){
                            Log.err("[验证器] lifecycleMonitor.captureAll(post-create) 崩溃: " + t.getMessage());
                        }
                    }

                    // Phase 5: Initialize content
                    if(initError.get() == null){
                        try{
                            content.init();
                        }catch(Throwable t){
                            Log.err("[验证器] content.init() 崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
                            initError.compareAndSet(null, new RuntimeException("content.init() 阶段崩溃: " + t.getMessage(), t));
                        }
                    }

                    // Phase 5.5-5.8: Analysis (only if init succeeded)
                    if(initError.get() == null){
                        try{
                            lifecycleMonitor.captureAll("post-init");
                            lifecycleMonitor.analyze();
                        }catch(Throwable t){
                            Log.err("[验证器] lifecycle 分析崩溃: " + t.getMessage());
                        }
                        try{
                            textureMonitor.checkAll();
                        }catch(Throwable t){
                            Log.err("[验证器] texture 检查崩溃: " + t.getMessage());
                        }
                        try{
                            renderMonitor.analyze();
                        }catch(Throwable t){
                            Log.err("[验证器] render 分析崩溃: " + t.getMessage());
                        }
                    }

                    // Phase 6: Check for content errors
                    if(Vars.mods.hasContentErrors()){
                        StringBuilder sb = new StringBuilder();
                        for(LoadedMod mod : Vars.mods.list()){
                            if(mod.hasContentErrors()){
                                for(Content cont : mod.erroredContent){
                                    sb.append("错误位于 ").append(cont.minfo.sourceFile.path())
                                      .append(": ").append(cont.minfo.baseError != null ? cont.minfo.baseError.getMessage() : "未知错误").append("\n");
                                }
                            }
                        }
                        // Don't set initError - we want to continue so we can report errors
                        errorLogs.add("检测到内容错误:\n" + sb);
                    }

                    // Phase 7: World + net + logic
                    try{
                        world = new World(){
                            @Override
                            public float getDarkness(int x, int y){
                                return 0;
                            }
                        };
                        net = new Net(null);

                        // Default rules: infinite resources (无限火力)
                        state.rules.infiniteResources = true;

                        Core.app.addListener(logic = new Logic());
                        Core.app.addListener(netServer = new NetServer());
                    }catch(Throwable t){
                        Log.err("[验证器] World/Net 初始化崩溃: " + t.getMessage());
                        initError.compareAndSet(null, new RuntimeException("World/Net 初始化崩溃: " + t.getMessage(), t));
                    }

                    // Phase 8: Base templates
                    if(initError.get() == null){
                        try{
                            bases.load();
                        }catch(Throwable t){
                            Log.err("[验证器] bases.load() 崩溃: " + t.getMessage());
                        }
                    }

                    // Phase 9: Mod init classes
                    if(initError.get() == null){
                        try{
                            mods.eachClass(Mod::init);
                        }catch(Throwable t){
                            Log.err("[验证器] Mod.init() 崩溃: " + t.getMessage());
                            initError.compareAndSet(null, new RuntimeException("Mod.init() 阶段崩溃: " + t.getMessage(), t));
                        }
                    }
                }

                @Override
                public void init(){
                    super.init();
                    begins[0] = true;
                    Core.app.exit();
                }
            };

            new HeadlessApplication(core, throwable -> {
                initError.compareAndSet(null, throwable);
            });

            long start = System.currentTimeMillis();
            while(!begins[0] && initError.get() == null && System.currentTimeMillis() - start < 120000){
                Thread.sleep(50);
            }

            if(initError.get() != null){
                throw initError.get();
            }

            if(!begins[0]){
                throw new TimeoutException("无头环境初始化超时（120秒）");
            }

            initialized.set(true);

        }catch(Throwable t){
            throw new RuntimeException("初始化无头环境失败", t);
        }
    }

    private void zipDirectory(Fi sourceDir, Fi destZip) throws Exception {
        try(java.util.zip.ZipOutputStream zos = new java.util.zip.ZipOutputStream(new java.io.FileOutputStream(destZip.file()))){
            Seq<Fi> files = new Seq<>();
            sourceDir.walk(f -> files.add(f));
            for(Fi f : files){
                String relative = f.path().substring(sourceDir.path().length() + 1);
                if(f.isDirectory()){
                    if(!relative.endsWith("/")) relative += "/";
                    zos.putNextEntry(new java.util.zip.ZipEntry(relative));
                    zos.closeEntry();
                }else{
                    zos.putNextEntry(new java.util.zip.ZipEntry(relative));
                    zos.write(f.readBytes());
                    zos.closeEntry();
                }
            }
        }
    }

    public LoadedMod getImportedMod(){ return importedMod; }
    public Seq<LoadedMod> getLoadedMods(){ return Vars.mods.list(); }
    public LoadedMod getMod(String name){ return Vars.mods.getMod(name); }
    public boolean hasContentErrors(){ return Vars.mods.hasContentErrors(); }

    public Map<String, List<String>> getContentErrors(){
        Map<String, List<String>> errors = new LinkedHashMap<>();
        for(LoadedMod mod : Vars.mods.list()){
            if(mod.hasContentErrors()){
                List<String> modErrors = new ArrayList<>();
                for(Content cont : mod.erroredContent){
                    modErrors.add("[" + cont.minfo.sourceFile.path() + "] " +
                        (cont.minfo.baseError != null ? cont.minfo.baseError.getMessage() : "unknown error"));
                }
                errors.put(mod.name, modErrors);
            }
        }
        return errors;
    }

    @SuppressWarnings("unchecked")
    public <T extends Content> Seq<T> getContent(ContentType type){
        return content.getBy(type);
    }

    public List<String> getErrorLogs(){ return Collections.unmodifiableList(errorLogs); }
    public List<String> getWarnLogs(){ return Collections.unmodifiableList(warnLogs); }
    public List<String> getAllLogs(){ return Collections.unmodifiableList(allLogs); }

    public World world(){ return Vars.world; }
    public ContentLoader content(){ return Vars.content; }
    public Mods mods(){ return Vars.mods; }

    public ContentLifecycleMonitor getLifecycleMonitor(){ return lifecycleMonitor; }
    public TextureResourceMonitor getTextureMonitor(){ return textureMonitor; }
    public RenderPipelineMonitor getRenderMonitor(){ return renderMonitor; }

    public void dispose(){
        if(initialized.get()){
            try{ Core.app.exit(); }catch(Exception ignored){}
            initialized.set(false);
        }
    }
}
