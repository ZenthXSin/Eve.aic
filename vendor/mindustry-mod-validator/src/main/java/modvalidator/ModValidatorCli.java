package modvalidator;

import modvalidator.report.*;

/**
 * CLI entry point for the mod validator.
 *
 * Usage:
 *   java -jar mod-validator.jar <mod-path> [--json] [--output <file>]
 *   java -jar mod-validator.jar --help
 */
public class ModValidatorCli {

    public static void main(String[] args){
        if(args.length == 0 || args[0].equals("--help") || args[0].equals("-h")){
            printHelp();
            return;
        }

        String modPath = null;
        boolean jsonOutput = false;
        String outputFile = null;

        for(int i = 0; i < args.length; i++){
            switch(args[i]){
                case "--json":
                    jsonOutput = true;
                    break;
                case "--output":
                case "-o":
                    if(i + 1 < args.length){
                        outputFile = args[++i];
                    }else{
                        System.err.println("错误: --output 需要指定文件路径");
                        System.exit(1);
                    }
                    break;
                case "--help":
                case "-h":
                    printHelp();
                    return;
                default:
                    if(!args[i].startsWith("-") && modPath == null){
                        modPath = args[i];
                    }else if(args[i].startsWith("-")){
                        System.err.println("未知选项: " + args[i]);
                        System.exit(1);
                    }
                    break;
            }
        }

        if(modPath == null){
            System.err.println("错误: 未指定模组路径。使用 --help 查看用法。");
            System.exit(1);
        }

        String testDataDir = System.getProperty("java.io.tmpdir") + "/modvalidator-test-data";
        ModValidator validator = new ModValidator(testDataDir);
        ReportGenerator reportGen = new ReportGenerator();

        try{
            System.out.println("正在验证模组: " + modPath);
            System.out.println("========================================");

            ValidationResult result = validator.validate(modPath);

            // Generate report
            String report = jsonOutput ? reportGen.generateJsonReport(result) : reportGen.generateTextReport(result);
            System.out.println(report);

            // Write to file if requested
            if(outputFile != null){
                reportGen.writeReport(result, outputFile, jsonOutput);
                System.out.println("报告已写入: " + outputFile);
            }

            // Exit code: 0 = passed, 1 = failed
            System.exit(result.hasErrors() ? 1 : 0);

        }catch(Exception e){
            System.err.println("致命错误: " + e.getMessage());
            e.printStackTrace();
            System.exit(2);
        }finally{
            validator.shutdown();
        }
    }

    private static void printHelp(){
        System.out.println("Mindustry 模组验证器 - 动态 JSON/JS 内容测试工具");
        System.out.println();
        System.out.println("用法:");
        System.out.println("  java -jar mod-validator.jar <模组路径> [选项]");
        System.out.println();
        System.out.println("参数:");
        System.out.println("  <模组路径>       模组文件(.zip)或目录的路径");
        System.out.println();
        System.out.println("选项:");
        System.out.println("  --json           以 JSON 格式输出报告");
        System.out.println("  --output, -o     将报告写入文件");
        System.out.println("  --help, -h       显示此帮助信息");
        System.out.println();
        System.out.println("退出码:");
        System.out.println("  0  验证通过");
        System.out.println("  1  验证失败（发现错误）");
        System.out.println("  2  致命错误");
        System.out.println();
        System.out.println("示例:");
        System.out.println("  java -jar mod-validator.jar ./我的模组");
        System.out.println("  java -jar mod-validator.jar ./我的模组.zip --json --output 报告.json");
    }
}
