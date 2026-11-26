import { describe, expect, it } from "vitest";
import { ApiRequestError } from "./client";
import { checkpointsByMessage, formatRewindSummary, isRewindConflict } from "./checkpoints";

describe("checkpoints helpers", () => {
  it("indexes checkpoints by message id", () => {
    const map = checkpointsByMessage([
      { id: "msg_2", bot_name: "助手", created_at: 200, files: ["b.ts"] },
      { id: "msg_1", bot_name: "助手", created_at: 100, files: ["a.ts", "c.ts"] },
    ]);
    expect(map.get("msg_1")?.files).toEqual(["a.ts", "c.ts"]);
    expect(map.get("msg_3")).toBeUndefined();
  });

  it("detects external edit conflict errors that allow force=true", () => {
    expect(
      isRewindConflict(
        new ApiRequestError(
          409,
          "conflict",
          "以下文件在检查点期间或之后被外部修改过，已取消回滚：a.rs。确认覆盖请传入 force=true。",
        ),
      ),
    ).toBe(true);
    expect(
      isRewindConflict(
        new ApiRequestError(409, "conflict", "这个检查点来自工作目录 /other，不能跨项目回滚。"),
      ),
    ).toBe(false);
  });

  it("formats rewind outcome summaries", () => {
    expect(
      formatRewindSummary({ checkpoint_id: "m1", restored: ["a.rs", "b.rs"], removed: ["new.rs"] }),
    ).toBe("已回滚：恢复 2 个文件，删除 1 个新建文件。");
    expect(formatRewindSummary({ checkpoint_id: "m2", restored: ["a.rs"], removed: [] })).toBe(
      "已回滚：恢复 1 个文件。",
    );
    expect(formatRewindSummary({ checkpoint_id: "m3", restored: [], removed: [] })).toBe(
      "已回滚，没有需要变更的文件。",
    );
  });
});
