// Vendored from oh-my-pi packages/collab-web/src/tool-render/tools/report-tool-issue.tsx @ 1ff12a7873c8d2eda8bb4447dd9df4dbd10377ce (MIT, see pwa/THIRD_PARTY_LICENSES).
/** `report_tool_issue` — automated QA grievance: which tool misbehaved + how. */
import type { ReactNode } from "react";
import { Badge, InvalidArg, Note, ResultText } from "../parts";
import type { ToolRenderer, ToolRenderProps } from "../types";
import { normalizeWs, str, truncate } from "../util";

function Summary({ args }: ToolRenderProps): ReactNode {
	const tool = str(args.tool);
	const report = str(args.report);
	if (!tool && !report) return <InvalidArg what="report" />;
	return (
		<span>
			{tool && <Badge tone="warn">{tool}</Badge>}
			{report && <span> {truncate(normalizeWs(report), 80)}</span>}
		</span>
	);
}

function Body({ args, result }: ToolRenderProps): ReactNode {
	const report = str(args.report);
	return (
		<>
			{report && <Note tone="warn">{report}</Note>}
			<ResultText result={result} maxLines={4} />
		</>
	);
}

export const reportToolIssueRenderer: ToolRenderer = { Summary, Body };
