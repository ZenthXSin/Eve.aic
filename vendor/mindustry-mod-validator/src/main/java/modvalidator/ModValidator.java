package modvalidator;

import arc.files.*;
import arc.struct.*;
import mindustry.*;
import mindustry.ctype.*;
import mindustry.mod.*;
import mindustry.mod.Mods.*;

import modvalidator.core.*;
import modvalidator.report.*;

import java.io.*;
import java.nio.file.*;
import java.util.ArrayList;
import java.util.LinkedHashMap;
import java.util.List;
import java.util.Map;
import java.util.concurrent.Future;
import java.util.concurrent.TimeoutException;

public class ModValidator {

    private final HeadlessTestEnvironment env;
    private final ReportGenerator reportGen;

    public ModValidator(String testDataDir){
        this.env = new HeadlessTestEnvironment(testDataDir);
        this.reportGen = new ReportGenerator();
    }

    public ValidationResult validate(String modPath){
        ValidationResult result = new ValidationResult();
        result.modPath = modPath;
        result.loadSuccess = true;

        long startTime = System.currentTimeMillis();

        try{
            System.out.println("[验证器] 正在初始化无头环境，模组: " + modPath);
            env.initialize(modPath);

            LoadedMod mod = env.getImportedMod();
            if(mod != null){
                result.modName = mod.name;
            }else{
                for(LoadedMod m : env.getLoadedMods()){
                    if(m.name != null && !m.name.isEmpty()){
                        result.modName = m.name;
                        break;
                    }
                }
            }

            long loadTime = System.currentTimeMillis() - startTime;
            result.loadTimeMs = loadTime;
            System.out.println("[验证器] 环境+模组准备完成，耗时 " + loadTime + "ms");

            if(env.hasContentErrors()){
                Map<String, List<String>> errors = env.getContentErrors();
                for(Map.Entry<String, List<String>> entry : errors.entrySet()){
                    for(String error : entry.getValue()){
                        result.addIssue(ValidationResult.Severity.ERROR, "content-parse", error);
                    }
                }
            }

            for(String err : env.getErrorLogs()){
                result.addIssue(ValidationResult.Severity.ERROR, "runtime-log", err);
            }
            for(String warn : env.getWarnLogs()){
                result.addIssue(ValidationResult.Severity.WARN, "runtime-log", warn);
            }

            System.out.println("[验证器] 正在运行动态测试...");
            long testStart = System.currentTimeMillis();

            ContentTester tester = new ContentTester(env, result);
            // 在独立线程中运行测试，防止死循环卡死主进程
            Future<?> future = java.util.concurrent.Executors.newSingleThreadExecutor().submit(() -> {
                try{
                    tester.runAllTests();
                }catch(Throwable t){
                    result.addIssue(ValidationResult.Severity.ERROR, "test",
                        "测试阶段崩溃: " + t.getClass().getSimpleName() + ": " + t.getMessage());
                }
            });
            try{
                future.get(60, java.util.concurrent.TimeUnit.SECONDS);
            }catch(java.util.concurrent.TimeoutException e){
                future.cancel(true);
                result.addIssue(ValidationResult.Severity.WARN, "test",
                    "动态测试总超时（60s），已取消");
            }catch(Throwable t){
                result.addIssue(ValidationResult.Severity.ERROR, "test",
                    "测试执行异常: " + t.getMessage());
            }

            result.testTimeMs = System.currentTimeMillis() - testStart;
            System.out.println("[验证器] 测试完成，耗时 " + result.testTimeMs + "ms");

        }catch(Throwable t){
            if(result.modName == null){
                LoadedMod m = env.getImportedMod();
                if(m != null) result.modName = m.name;
            }
            String category;
            if(t instanceof FileNotFoundException){
                category = "模组文件未找到";
            }else if(t instanceof TimeoutException){
                category = "加载超时";
            }else if(t instanceof StackOverflowError){
                category = "栈溢出（疑似无限递归）";
            }else{
                category = "意外错误";
            }
            result.addIssue(ValidationResult.Severity.CRITICAL, "load",
                category + ": " + t.getClass().getSimpleName() + ": " + t.getMessage());
            StringWriter sw = new StringWriter();
            t.printStackTrace(new PrintWriter(sw));
            String trace = sw.toString();
            // 截取关键堆栈（去掉重复递归帧）
            if(t instanceof StackOverflowError){
                trace = compressStackTrace(trace, 60);
            }else{
                trace = trace.substring(0, Math.min(2000, trace.length()));
            }
            result.addIssue(ValidationResult.Severity.INFO, "debug",
                "堆栈跟踪: " + trace);
        }

        return result;
    }

    /**
     * 压缩 StackOverflowError 的堆栈：去掉重复递归帧，保留头部+尾部+递归摘要
     */
    private static String compressStackTrace(String trace, int maxUniqueFrames){
        String[] lines = trace.split("\n");
        StringBuilder sb = new StringBuilder();

        // 保留前 N 行（异常头 + 首批帧）
        int head = Math.min(maxUniqueFrames, lines.length);
        for(int i = 0; i < head; i++){
            sb.append(lines[i]).append("\n");
        }

        // 检测递归模式：统计重复的 at 行
        if(lines.length > maxUniqueFrames){
            Map<String, Integer> freq = new LinkedHashMap<>();
            for(int i = 0; i < lines.length; i++){
                String line = lines[i].trim();
                if(line.startsWith("at ")){
                    freq.merge(line, 1, Integer::sum);
                }
            }

            // 找出出现次数 > 3 的帧（递归特征）
            List<String> recursiveFrames = new ArrayList<>();
            for(var entry : freq.entrySet()){
                if(entry.getValue() > 3){
                    recursiveFrames.add("  ↻ " + entry.getKey() + " ×" + entry.getValue() + " (递归)\n");
                }
            }

            if(!recursiveFrames.isEmpty()){
                sb.append("  ... 递归摘要（出现 >3 次的帧）:\n");
                for(String rf : recursiveFrames){
                    sb.append(rf);
                }
            }

            // 保留最后几行
            int tailStart = Math.max(head, lines.length - 5);
            sb.append("  ... （省略 ").append(lines.length - head - 5).append(" 行）\n");
            for(int i = tailStart; i < lines.length; i++){
                sb.append(lines[i]).append("\n");
            }
        }

        return sb.toString();
    }

    public void shutdown(){
        env.dispose();
    }
}
