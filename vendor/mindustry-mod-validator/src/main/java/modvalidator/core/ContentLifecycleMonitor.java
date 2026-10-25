package modvalidator.core;

import arc.struct.*;
import mindustry.ctype.*;
import mindustry.gen.*;
import mindustry.world.*;

import java.lang.reflect.*;
import java.util.*;

/**
 * Monitors Content lifecycle (constructor → init → postInit → loadIcon → load)
 * by capturing field snapshots at each stage and detecting anomalies.
 */
public class ContentLifecycleMonitor {

    private final Map<String, ContentSnapshot> snapshots = new LinkedHashMap<>();
    private final List<String> lifecycleLog = new ArrayList<>();
    private final List<FieldAnomaly> anomalies = new ArrayList<>();

    /** Capture a field snapshot of a Content object at a given lifecycle stage. */
    public void captureStage(Content content, String stage){
        String key = content.getContentType().name() + ":" + contentName(content);
        ContentSnapshot snap = snapshots.computeIfAbsent(key, k -> new ContentSnapshot(key));
        snap.stages.put(stage, extractFields(content));
        lifecycleLog.add("[" + stage + "] " + key);
    }

    /** Capture snapshots for all currently registered mod content. */
    public void captureAll(String stage){
        for(ContentType type : ContentType.all){
            for(Content content : mindustry.Vars.content.getBy(type)){
                if(content.isModded()){
                    captureStage(content, stage);
                }
            }
        }
    }

    /** Compare init vs constructor, load vs init — detect suspicious changes. */
    public void analyze(){
        for(ContentSnapshot snap : snapshots.values()){
            Map<String, Object> ctor = snap.stages.get("constructor");
            Map<String, Object> init = snap.stages.get("init");
            Map<String, Object> load = snap.stages.get("load");

            if(ctor != null && init != null){
                detectAnomalies(snap.key, "constructor→init", ctor, init);
            }
            if(init != null && load != null){
                detectAnomalies(snap.key, "init→load", init, load);
            }

            // Check for null critical fields after init
            if(init != null){
                for(Map.Entry<String, Object> e : init.entrySet()){
                    if(e.getValue() == null && isCriticalField(snap.key, e.getKey())){
                        anomalies.add(new FieldAnomaly(snap.key, e.getKey(), FieldAnomalyType.NULL_CRITICAL, "字段在 init 后为 null"));
                    }
                }
            }

            // Check for numeric anomalies (negative capacities, etc.)
            if(init != null){
                checkNumericAnomalies(snap.key, init);
            }
        }
    }

    private void detectAnomalies(String key, String phase, Map<String, Object> before, Map<String, Object> after){
        for(Map.Entry<String, Object> e : after.entrySet()){
            String field = e.getKey();
            Object afterVal = e.getValue();
            Object beforeVal = before.get(field);

            if(beforeVal != null && afterVal == null){
                anomalies.add(new FieldAnomaly(key, field, FieldAnomalyType.BECAME_NULL,
                    phase + ": " + beforeVal + " → null"));
            }
        }
    }

    private void checkNumericAnomalies(String key, Map<String, Object> fields){
        // Check for negative capacities
        checkNegative(key, fields, "liquidCapacity");
        checkNegative(key, fields, "itemCapacity");
        checkNegative(key, fields, "health", -1); // -1 means auto-calculate, that's fine

        // Check for zero/negative buildTime (might be intentional for instantBuild)
        Object buildTime = fields.get("buildTime");
        if(buildTime instanceof Float f && f < 0 && !hasFieldTrue(fields, "instantBuild")){
            anomalies.add(new FieldAnomaly(key, "buildTime", FieldAnomalyType.SUSPICIOUS_VALUE,
                "buildTime=" + f + " 但 instantBuild=false"));
        }
    }

    private void checkNegative(String key, Map<String, Object> fields, String fieldName){
        checkNegative(key, fields, fieldName, 0);
    }

    private void checkNegative(String key, Map<String, Object> fields, String fieldName, float threshold){
        Object val = fields.get(fieldName);
        if(val instanceof Float f && f < threshold){
            anomalies.add(new FieldAnomaly(key, fieldName, FieldAnomalyType.NEGATIVE_VALUE,
                fieldName + "=" + f));
        }else if(val instanceof Integer i && i < (int)threshold){
            anomalies.add(new FieldAnomaly(key, fieldName, FieldAnomalyType.NEGATIVE_VALUE,
                fieldName + "=" + i));
        }
    }

    private boolean hasFieldTrue(Map<String, Object> fields, String fieldName){
        return fields.get(fieldName) instanceof Boolean b && b;
    }

    private boolean isCriticalField(String key, String fieldName){
        // Fields that should never be null after init
        return switch(fieldName){
            case "name", "region" -> true;
            default -> false;
        };
    }

    @SuppressWarnings("unchecked")
    private Map<String, Object> extractFields(Object obj){
        Map<String, Object> result = new LinkedHashMap<>();
        Class<?> clazz = obj.getClass();
        while(clazz != null && clazz != Object.class){
            for(Field f : clazz.getDeclaredFields()){
                if(Modifier.isStatic(f.getModifiers())) continue;
                if(Modifier.isTransient(f.getModifiers())) continue;
                try{
                    f.setAccessible(true);
                    Object val = f.get(obj);
                    // Only capture primitive/wrapper/String values and arrays
                    if(isSimpleType(val)){
                        result.put(f.getName(), val);
                    }
                }catch(Exception ignored){}
            }
            clazz = clazz.getSuperclass();
        }
        return result;
    }

    private boolean isSimpleType(Object val){
        if(val == null) return true;
        return val instanceof Number || val instanceof String || val instanceof Boolean ||
               val instanceof Character || val instanceof Enum || val instanceof Class ||
               (val.getClass().isArray() && val.getClass().getComponentType().isPrimitive());
    }

    private String contentName(Content c){
        if(c instanceof MappableContent mc) return mc.name;
        return c.toString();
    }

    public List<String> getLifecycleLog(){ return lifecycleLog; }
    public List<FieldAnomaly> getAnomalies(){ return anomalies; }
    public Map<String, ContentSnapshot> getSnapshots(){ return snapshots; }

    /** Represents a single content object's lifecycle data. */
    public static class ContentSnapshot {
        public final String key;
        public final Map<String, Map<String, Object>> stages = new LinkedHashMap<>();

        public ContentSnapshot(String key){ this.key = key; }
    }

    /** Represents a detected field anomaly. */
    public record FieldAnomaly(String contentKey, String fieldName, FieldAnomalyType type, String detail){}

    public enum FieldAnomalyType {
        BECAME_NULL, NULL_CRITICAL, NEGATIVE_VALUE, SUSPICIOUS_VALUE
    }
}
