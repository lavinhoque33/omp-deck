// Vendored from oh-my-pi packages/collab-web/src/tool-render/tools/wait.tsx @ 1ff12a7873c8d2eda8bb4447dd9df4dbd10377ce (MIT, see pwa/THIRD_PARTY_LICENSES).
import type { ReactNode } from "react";
import type { ToolRenderer, ToolRenderProps } from "../types";
import { detailsRecord } from "../util";
import { ircRenderer } from "./irc";
import { jobRenderer } from "./job";

/** A consumed peer message renders as IRC; otherwise show the job snapshot. */
function Summary(props: ToolRenderProps): ReactNode {
	return detailsRecord(props.result)?.waited ? (
		<ircRenderer.Summary {...props} args={{ op: "wait" }} />
	) : (
		<jobRenderer.Summary {...props} args={{ poll: [] }} />
	);
}

function Body(props: ToolRenderProps): ReactNode {
	return detailsRecord(props.result)?.waited ? (
		ircRenderer.Body ? (
			<ircRenderer.Body {...props} args={{ op: "wait" }} />
		) : null
	) : jobRenderer.Body ? (
		<jobRenderer.Body {...props} args={{ poll: [] }} />
	) : null;
}

export const waitRenderer: ToolRenderer = { Summary, Body };
