<script lang="ts">
	import { getContext } from "svelte";
	import FloatingMenu from "/src/components/layout/FloatingMenu.svelte";
	import LayoutRow from "/src/components/layout/LayoutRow.svelte";
	import ShortcutLabel from "/src/components/widgets/labels/ShortcutLabel.svelte";
	import TextLabel from "/src/components/widgets/labels/TextLabel.svelte";
	import type { TooltipStore } from "/src/stores/tooltip";
	import { parseMarkdown } from "/src/utility-functions/markdown";
	import type { EditorWrapper, LabeledShortcut } from "/wrapper/pkg/graphite_wasm_wrapper";

	const tooltip = getContext<TooltipStore>("tooltip");
	const editor = getContext<EditorWrapper>("editor");

	let self: FloatingMenu | undefined;

	$: label = parseMarkdown(filterTodo($tooltip.element?.getAttribute("data-tooltip-label")?.trim()));
	$: description = parseMarkdown(filterTodo($tooltip.element?.getAttribute("data-tooltip-description")?.trim()));
	// How the described thing is written, like a function's signature, shown in the header where a shortcut goes
	$: code = $tooltip.element?.getAttribute("data-tooltip-code")?.trim();
	$: shortcutJSON = $tooltip.element?.getAttribute("data-tooltip-shortcut")?.trim();
	$: shortcut = ((shortcutJSON) => {
		if (!shortcutJSON) return undefined;
		try {
			const parsed: LabeledShortcut = JSON.parse(shortcutJSON);
			if (!Array.isArray(parsed)) return undefined;

			return parsed;
		} catch {
			return undefined;
		}
	})(shortcutJSON);

	// TODO: Once all TODOs are replaced with real text, remove this function
	function filterTodo(text: string | undefined): string | undefined {
		if (text?.trim().toUpperCase() === "TODO" && !editor.inDevelopmentMode()) return "";
		return text;
	}
</script>

{#if label || description}
	<div class="tooltip" style:top={`${$tooltip.position.y}px`} style:left={`${$tooltip.position.x}px`}>
		<FloatingMenu open={true} type="Tooltip" direction="Bottom" bind:this={self}>
			{#if label || shortcut || code}
				<LayoutRow class="tooltip-header">
					{#if label}
						<TextLabel class="tooltip-label">{@html label}</TextLabel>
					{/if}
					{#if shortcut}
						<ShortcutLabel shortcut={{ shortcut }} />
					{/if}
					{#if code}
						<TextLabel class="tooltip-code" monospace={true}>{code}</TextLabel>
					{/if}
				</LayoutRow>
			{/if}
			{#if description}
				<TextLabel class="tooltip-description">{@html description}</TextLabel>
			{/if}
		</FloatingMenu>
	</div>
{/if}

<style lang="scss">
	.tooltip {
		position: absolute;
		pointer-events: none;
		width: 0;
		height: 0;

		.floating-menu-content {
			max-width: Min(400px, 50vw);

			.tooltip-header + .tooltip-description {
				margin-top: 4px;
			}

			.text-label {
				white-space: pre-wrap;
			}

			.text-label + .shortcut-label {
				margin-left: 8px;
			}

			.tooltip-code {
				margin-left: auto;
				padding-left: 8px;
				color: var(--color-b-lightgray);
			}

			.tooltip-description {
				color: var(--color-b-lightgray);
			}
		}
	}
</style>
