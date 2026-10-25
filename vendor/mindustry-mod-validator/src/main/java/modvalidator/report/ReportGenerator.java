package modvalidator.report;

import modvalidator.ValidationResult;

import java.io.*;
import java.nio.file.*;
import java.time.*;
import java.time.format.*;

/**
 * Generates validation reports in multiple formats.
 */
public class ReportGenerator {

    /**
     * Generate a human-readable text report.
     */
    public String generateTextReport(ValidationResult result){
        StringBuilder sb = new StringBuilder();
        sb.append("========================================\n");
        sb.append("  Mindustry 模组验证报告\n");
        sb.append("========================================\n\n");

        sb.append("模组: ").append(result.modName != null ? result.modName : "(未知)").append("\n");
        sb.append("路径: ").append(result.modPath).append("\n");
        sb.append("时间: ").append(LocalDateTime.now().format(DateTimeFormatter.ISO_LOCAL_DATE_TIME)).append("\n");
        sb.append("加载耗时: ").append(result.loadTimeMs).append("ms\n");
        sb.append("测试耗时: ").append(result.testTimeMs).append("ms\n");
        sb.append("状态: ").append(result.hasErrors() ? "失败" : "通过").append("\n");

        sb.append("\n--- Summary ---\n");
        sb.append("  错误: ").append(result.errorCount()).append("\n");
        sb.append("  警告: ").append(result.warnCount()).append("\n");
        sb.append("  信息: ").append(result.infoCount()).append("\n");

        if(!result.issues.isEmpty()){
            sb.append("\n--- Issues ---\n");
            for(ValidationResult.Issue issue : result.issues){
                sb.append("  ").append(issue.toString()).append("\n");
            }
        }

        sb.append("\n========================================\n");
        return sb.toString();
    }

    /**
     * Generate a JSON report.
     */
    public String generateJsonReport(ValidationResult result){
        StringBuilder sb = new StringBuilder();
        sb.append("{\n");
        sb.append("  \"modName\": ").append(quote(result.modName)).append(",\n");
        sb.append("  \"modPath\": ").append(quote(result.modPath)).append(",\n");
        sb.append("  \"timestamp\": ").append(quote(LocalDateTime.now().format(DateTimeFormatter.ISO_LOCAL_DATE_TIME))).append(",\n");
        sb.append("  \"loadTimeMs\": ").append(result.loadTimeMs).append(",\n");
        sb.append("  \"testTimeMs\": ").append(result.testTimeMs).append(",\n");
        sb.append("  \"status\": ").append(quote(result.hasErrors() ? "失败" : "通过")).append(",\n");
        sb.append("  \"summary\": {\n");
        sb.append("    \"errors\": ").append(result.errorCount()).append(",\n");
        sb.append("    \"warnings\": ").append(result.warnCount()).append(",\n");
        sb.append("    \"info\": ").append(result.infoCount()).append("\n");
        sb.append("  },\n");
        sb.append("  \"issues\": [\n");

        for(int i = 0; i < result.issues.size(); i++){
            ValidationResult.Issue issue = result.issues.get(i);
            sb.append("    {\n");
            sb.append("      \"severity\": ").append(quote(issue.severity.name())).append(",\n");
            sb.append("      \"category\": ").append(quote(issue.category)).append(",\n");
            sb.append("      \"message\": ").append(quote(issue.message)).append(",\n");
            sb.append("      \"detail\": ").append(quote(issue.detail)).append("\n");
            sb.append("    }");
            if(i < result.issues.size() - 1) sb.append(",");
            sb.append("\n");
        }

        sb.append("  ]\n");
        sb.append("}\n");
        return sb.toString();
    }

    /**
     * Write a report to a file.
     */
    public void writeReport(ValidationResult result, String outputPath, boolean json) throws IOException{
        String content = json ? generateJsonReport(result) : generateTextReport(result);
        Files.writeString(Path.of(outputPath), content);
    }

    private String quote(String s){
        if(s == null) return "null";
        return "\"" + s.replace("\\", "\\\\").replace("\"", "\\\"").replace("\n", "\\n").replace("\r", "\\r") + "\"";
    }
}
