package modvalidator;

import java.util.*;

/**
 * Result of a mod validation run.
 */
public class ValidationResult {

    public enum Severity {
        OK, INFO, WARN, ERROR, CRITICAL
    }

    public static class Issue {
        public final Severity severity;
        public final String category;
        public final String message;
        public final String detail;

        public Issue(Severity severity, String category, String message, String detail){
            this.severity = severity;
            this.category = category;
            this.message = message;
            this.detail = detail;
        }

        public Issue(Severity severity, String category, String message){
            this(severity, category, message, null);
        }

        @Override
        public String toString(){
            return "[" + severity + "] [" + category + "] " + message + (detail != null ? "\n  " + detail : "");
        }
    }

    public String modName;
    public String modPath;
    public boolean loadSuccess;
    public final List<Issue> issues = new ArrayList<>();
    public long loadTimeMs;
    public long testTimeMs;

    public void addIssue(Severity severity, String category, String message, String detail){
        issues.add(new Issue(severity, category, message, detail));
        if(severity == Severity.ERROR || severity == Severity.CRITICAL){
            loadSuccess = false;
        }
    }

    public void addIssue(Severity severity, String category, String message){
        addIssue(severity, category, message, null);
    }

    public boolean hasErrors(){
        return issues.stream().anyMatch(i -> i.severity == Severity.ERROR || i.severity == Severity.CRITICAL);
    }

    public long errorCount(){
        return issues.stream().filter(i -> i.severity == Severity.ERROR || i.severity == Severity.CRITICAL).count();
    }

    public long warnCount(){
        return issues.stream().filter(i -> i.severity == Severity.WARN).count();
    }

    public long infoCount(){
        return issues.stream().filter(i -> i.severity == Severity.INFO).count();
    }
}
