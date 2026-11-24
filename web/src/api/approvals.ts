import type { ApprovalDecision, StepApproval } from "./types";

export interface ApprovalChoice {
  decision: ApprovalDecision;
  label: string;
  hint: string;
}

const CHOICES: ApprovalChoice[] = [
  { decision: "once", label: "允许一次", hint: "只允许这一次" },
  { decision: "always", label: "总是允许", hint: "这个房间里以后同样的操作不再问" },
  {
    decision: "task",
    label: "这个任务里的命令都允许",
    hint: "这个任务之后执行命令都不再问；推送和用 gh 的命令照样问",
  },
  { decision: "deny", label: "拒绝", hint: "不执行，Bot 会被告知" },
];

/** 这张审批卡上有哪几个按钮。「总是允许」和「这个任务里的命令都允许」由服务端说了算。 */
export function approvalChoices(approval: StepApproval): ApprovalChoice[] {
  return CHOICES.filter(({ decision }) => {
    if (decision === "always") return approval.can_allow_always;
    if (decision === "task") return approval.can_allow_task;
    return true;
  });
}
