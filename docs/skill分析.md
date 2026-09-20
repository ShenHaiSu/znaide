# Skill 功能与逻辑分析

> 分析对象：当前项目 `znaide` 下 Skill 子系统
> 源码位置：`crates/core/src/skills.rs`、`crates/core/src/session.rs`、`crates/tui/src/app.rs`、`crates/cli/src/main.rs`、`crates/core/src/config.rs`
> 文档位置：`docs/使用说明.md` 第 7 节、`docs/项目分析.md`
> 示例资产：`skills示例/{archive-downloads,clean-junk,weekly-report}/`

---

## 1. 总览：一句话

**Skill = 一个目录 + 一个 `SKILL.md`（frontmatter + 正文模板）+ 可选入口脚本。**

- 正文是给模型看的“操作说明书”，支持 `{args}` 占位替换。
- 可选 `entry` 入口脚本在调用时自动执行一次，输出并入上下文。
- 触发方式两种：模型自主调用 `skill` 工具 / 用户手动 `/技能名 [参数]`。
- 权限无新增面：内部所有写文件 / 跑命令仍走统一权限档位、高危判定与 undo。

---

## 2. 数据模型

### 2.1 `Skill` 结构（`core/src/skills.rs:125-140`）

```rust
pub struct Skill {
    pub name: String,                 // = 目录名，唯一标识，也用于 /命令 与模型 enum
    pub description: String,          // 一句话说明，给 /skills 展示 + 模型判断何时用
    pub disable_model_invocation: bool, // true = 不暴露给模型，只能手动 /名字 触发
    pub entry: Option<String>,        // 入口脚本相对路径（skill 目录内），None = 无入口
    pub body: String,                 // 正文模板，可含 {args}
    pub dir: PathBuf,                 // SKILL.md 所在目录
    pub source: SkillSource,          // Builtin / User / Project
}
```

关键方法：

- `render(args)`: `body.replace("{args}", args)`（`skills.rs:144`）
- `entry_abs()`: `dir.join(entry)` 取绝对路径（`skills.rs:149`）

### 2.2 `SkillSource` 三层（`skills.rs:107-123`）

| 层级 | 目录 | label | 优先级 |
|------|------|-------|--------|
| Builtin 内置 | `~/.znaide/builtin-skills/` | `内置` | 最低 |
| User 用户/全局 | `~/.znaide/skills/`（`data_dir().join("skills")`） | `用户` | 中 |
| Project 项目 | `<cwd>/.znaide/skills/` | `项目` | 最高，同名覆盖 |

`data_dir()` 定义见 `core/src/config.rs:5-12`：默认 `~/.znaide`，`ZNAIDE_DATA_DIR` 环境变量可覆盖（测试/多实例用）。

### 2.3 `SKILL.md` 格式

frontmatter 简子集，手写解析，不引 yaml 依赖（`parse_frontmatter`, `skills.rs:280-302`）：

```markdown
---
name: demo              # 可选，若写须与目录名一致，否则整包跳过+告警
description: 一句话说明  # 推荐，出现在 /skills、模型判断依据
disable-model-invocation: true  # 可选，true 则只能手动触发
entry: scripts/run.sh   # 可选，须为技能目录内相对路径，不得含 .. / 绝对路径
---

正文模板，可含 {args}，调用时替换为实际参数
```

校验规则（`load_skill`, `skills.rs:222-268`）：

- 目录名合法性 `valid_name()`：非空、≤40 字符、不以 `-` 开头、只含 ASCII 字母/数字/`_`/`-`。
- 无 `SKILL.md` → 跳过 + warning。
- `frontmatter.name != 目录名` → 跳过 + warning。
- `entry` 为绝对路径或含 `ParentDir(..)` → 整包拒绝 + warning。

`warnings` 会在 `/skills` 输出尾部以 `⚠` 列出，便于发现坏文件。

---

## 3. 扫描与覆盖逻辑

### 3.1 生产入口 `scan(cwd)`（`skills.rs:161-169`）

```rust
pub fn scan(cwd: &Path) -> SkillSet {
    scan_dir_into(&mut map, &mut warnings, &builtin_dir(), SkillSource::Builtin);
    scan_dir_into(&mut map, &mut warnings, &data_dir().join("skills"), SkillSource::User);
    scan_dir_into(&mut map, &mut warnings, &cwd.join(".znaide").join("skills"), SkillSource::Project);
    finish_skill_set(map, warnings)
}
```

- `scan_dir_into()`：`read_dir` 遍历一级子目录，每个子目录调 `load_skill()`，成功则 `map.insert(name, skill)` —— **后扫直接覆盖，同名 Project 赢**。
- `finish_skill_set()`：按 `name` 排序返回 `SkillSet { skills, warnings }`。
- 测试注入用 `scan_with_dirs(user_dir, project_dir)`（`skills.rs:172-178`），不含 builtin 层。

### 3.2 内置物化 `ensure_builtin_skills()`（`skills.rs:88-105`）

- `BUILTIN_ASSETS` 编译期 `include_str!` 内嵌 `skills示例/` 下三个技能（含 `.sh/.ps1` 脚本），改示例目录即改内置，单一来源。
- 启动时（`cli/src/main.rs:127`）物化到 `~/.znaide/builtin-skills/`，**缺失才写，不覆盖用户改动**。
- 内置技能可改可抄：想改直接编辑物化副本，想当模板复制到用户级目录再改。

### 3.3 旧 commands 迁移（`skills.rs:421-468`）

- 启动时 `migrate_legacy_commands()` 把旧 `~/.znaide/commands/*.md` 平移成 `skills/<名>/SKILL.md`。
- 前提：`skills/` 为空，否则不覆盖返回 0。
- 迁移后默认 `disable-model-invocation: true`，保住旧“仅手动 /命令”语义，源 `.md` 删除。

---

## 4. 调用链：两种触发共用同一运行器

### 4.1 模型自主调用

1. 每回合 `run_turn()` 开头组工具定义（`session.rs:846-851`）：
   `ToolRegistry::builtin().defs() + build_skill_tool_def(cwd) + mcp.tool_defs()`。
2. `build_skill_tool_def()`（`session.rs:1488-1517`）：
   - `scan(cwd)` + `model_visible()` 过滤掉 `disable_model_invocation`。
   - 为空返回 `None`（模型看不到 skill 工具）。
   - 否则组单个 `skill` 工具：`name enum + args string`，描述前 30 个 `"- 名字: 描述"`，超 30 截断 `…(共 N 个)`。**上下文不随技能数膨胀、命名无冲突。**
3. 模型调 `skill(name, args)` → `session.rs:1131` 分发到 `run_skill(name, args)`。
4. UI 工具卡片显示 `skill「技能名」` 而非笼统 `skill`（`session.rs:1143-1146`，`tui` 文档亦有说明）。

### 4.2 手动触发 `/技能名 [参数]`

` tui/src/app.rs:2032-2056`：

- 非内置命令一律当技能查：`scan(cwd)` + `find(name)`。
- 命中：推 `▶ 技能「xx」已触发: 描述(来源)` Notice。
  - 有 `entry` → 返回 `SlashOutcome::RunSkill { name, args }`，经 `session.run_skill_turn()` 跑（需走 entry 权限确认，UI 预提示“可能需要你确认”）。
  - 无 `entry` → `skill.render(args)` 直接当本回合 prompt（旧自定义命令同款行为），返回 `SlashOutcome::Task(prompt)`。
- 未命中：`未知命令: xx / 输入 /help ...`，有技能时追加 `输入 /skills 可查看`。

`/skills` 列表（`app.rs:2075-2103`）：`scan(cwd)` 全量列出 ` /名 — 描述(来源)[仅手动]` + warnings + 尾注目录说明。

补全：`/` 弹命令菜单含已安装技能（`docs/使用说明.md:67`），`app.rs:2794` 实时 `scan` 供补全。

### 4.3 唯一运行器 `run_skill()`（`session.rs:1309-1374`）

```text
scan(cwd) → find(name) → render(args) → [entry? 组命令→权限判定→执行→拼输出] → (out, ok)
```

- 未找到：可用列表提示，无技能时提示参考 README 示例。
- 有 `entry`：
  1. `entry_command(skill, args)` 组命令，失败则拼 `[入口脚本未执行:原因](本次仅按正文执行)`。
  2. `check_permission(PermKind::Command, &cmd)` 三档判定，Denied 则拼 `[入口脚本未执行:原因]`。
  3. Allowed 则复用 `tools::execute("run_shell_command", ...)` 通道（bash/cmd、超时、输出截断）。YOLO 会话透传 YOLO（跳过高危判定），否则以 `BypassPermissions` 跑，默认静默预算 90s。
  4. 成功拼 `[入口脚本输出]\n...`，失败拼 `[入口脚本执行失败:...]`。
- `run_skill_turn(name, args)`（`session.rs:1378-1395`）：调 `run_skill`，失败只 `notify` 不开回合（免 UI busy 没人清）；成功以 `content` 开新 `run_turn`。

---

## 5. Entry 跨平台

- Unix（`skills.rs:353-368`）：`SKILL_DIR='<dir>' bash -- '<entry>' '<args>'`，要求脚本文件存在，`sh_quote` 单引号转义。
- Windows（`skills.rs:377-414`）：`win_entry_candidates()` 声明 `.sh` 时按 `.ps1 → .bat → .cmd → 原声明` 找同名脚本；`.ps1` 用 `powershell -NoProfile -ExecutionPolicy Bypass -File`，`.bat/.cmd` 用 `call`，其它直接运行；`SKILL_DIR` 用 `set "SKILL_DIR=..."&&` 传入。
- 调用参数 `{args}` 经命令行传入；脚本可读 `SKILL_DIR` 定位自身附件。
- 正文建议写跨平台指令（区分 `mv` ↔ `move`/`Move-Item`）。

---

## 6. 同一会话是否有热加载？

**结论：有，无缓存的“每用现扫”，新增/修改下个回合即生效，无需重启。**

依据：

- `session.rs:848` 每回合组 defs 时 `scan(cwd)` 一次。
- `session.rs:1310` 每次 `run_skill` 再 `scan` 一次。
- `tui/app.rs:2034/2076/2794` 每次斜杠分发、`/skills`、补全都 `scan`。
- 文档原话（`docs/使用说明.md:204`）：“技能是每回合动态扫描的，无需重启。”

注意边界：

- 同一 `run_turn` 内多轮 tool loop 的 `defs` 是回合初快照，中途新增技能本轮模型看不到，下一用户消息回合可见；手动 `/技能名` 不受此限，每次现扫。
- 内置示例改了需重新编译（`include_str!` 编译期内嵌），运行时改物化副本即时生效。

---

## 7. Global 与 Project 级读写

### 7.1 读：原生支持，一次合并

`scan(cwd)` 一次返回三层合并列表，`/skills` 标注来源（内置/用户/项目）与 `[仅手动]`。

| 级别 | 路径 | 说明 |
|------|------|------|
| Global/用户级 | `~/.znaide/skills/<名>/SKILL.md`（Windows `%USERPROFILE%\.znaide\skills\`） | 推荐个人常用，跨项目生效；`ZNAIDE_DATA_DIR` 可整体改根 |
| Project 项目级 | `<cwd>/.znaide/skills/<名>/SKILL.md` | 随仓库分发，当前项目示例：`F:\MyProgram\rust\znaide\.znaide\skills\`；同名覆盖用户级 |
| Builtin | `~/.znaide/builtin-skills/` | 模板副本，能改但别当 global 用 |

### 7.2 写：无专用 skill 读写工具，用通用文件工具直接操作目录

- Skill 系统自身只读（scan/render/execute），未提供 `skill install/write` 专用工具。
- 读写靠通用工具：`list_directory / read_file / write_file / edit / glob / grep_search / run_shell_command` 直接操作上述两级目录即可。
  - 新建 `~/.znaide/skills/foo/SKILL.md` = 装全局技能。
  - 新建 `.znaide/skills/foo/SKILL.md` = 装项目技能。
- 写文件走正常权限三档 + undo 快照（`write_file/edit` 写前必快照，见 `docs/项目分析.md:96`）。
- 启动 `ensure_data_dirs()` 会建 `memories/skills/sessions` 空目录（`config.rs:1012-1018`），`skills/` 不存在 `scan` 直接跳过，无需预建。

### 7.3 安全提醒

- 项目级（第三方仓库自带）`SKILL.md` 正文属于不可信输入，需警惕夹带“越权/删改/外发”指令（`docs/使用说明.md:240`）。
- 项目级 entry 执行前同样走权限确认；MCP / 项目级 skill 统一视为不可信（`docs/项目分析.md:96`）。

---

## 8. 调试清单

- `/skills` 为空 → 检查是否为 `skills/<名字>/SKILL.md` 两层结构、目录名合法性、frontmatter `name` 一致性、warnings 黄字。
- 想调试 → 先 `/技能名` 手动触发，看入口输出与模型行为（`docs/使用说明.md:246`）。
- Windows 入口不跑 → 确认同目录有同名 `.ps1`（或 `.bat/.cmd`）；提示“当前平台没有可用入口”则按正文仍可执行。
- 模型不调 → 检查是否 `disable-model-invocation: true`（仅手动），或描述是否让模型能匹配（说一句符合描述的话，如“把下载目录整理一下”）。

---

## 9. 相关测试

- `core/src/skills.rs` 单元测试：内置资产完整、frontmatter 解析、`render`、`scan_project_overrides_user_and_filters_disable`、entry 相对性、`entry_command` 引号、`win_entry_candidates`、旧迁移。
- `core/tests/mock_e2e.rs:984-1110`：`scaffold_demo_skill` 在 `<tmp>/.znaide/skills/demo` 搭项目级技能，回归模型自主 `skill(name enum)` → 渲染正文 + entry 输出回填；未知名优雅失败不发网络请求。
