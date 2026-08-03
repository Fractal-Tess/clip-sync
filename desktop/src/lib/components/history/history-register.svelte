<script lang="ts">
	import { Clipboard, Filter, Pin, PinOff, Play, Trash2 } from '@lucide/svelte';
	import { onMount } from 'svelte';

	import type { HistoryItem } from '$lib/bridge';
	import HistoryImagePreview from '$lib/components/history-image-preview.svelte';
	import { Button } from '$lib/components/ui/button';
	import {
		ContextMenu,
		ContextMenuContent,
		ContextMenuItem,
		ContextMenuLabel,
		ContextMenuSeparator,
		ContextMenuShortcut,
		ContextMenuTrigger
	} from '$lib/components/ui/context-menu';
	import {
		Empty,
		EmptyDescription,
		EmptyHeader,
		EmptyMedia,
		EmptyTitle
	} from '$lib/components/ui/empty';
	import { Skeleton } from '$lib/components/ui/skeleton';
	import { formatAge, formatBytes } from '$lib/format';
	import { historyItemHasImage } from '$lib/history-image-cache.svelte';
	import type { HistoryImageState } from '$lib/history-image-types';

	let {
		items,
		previews,
		loading,
		refreshing,
		historyLoaded,
		total,
		pageSize,
		columns,
		rows,
		query,
		activating,
		selectedIndex = $bindable(),
		historyGrid = $bindable(null),
		historyWorkspace = $bindable(null),
		onClearSearch,
		onActivate,
		onPin,
		onDelete,
		onFilterSource,
		onNavigate,
		onRequestImage
	}: {
		items: HistoryItem[];
		previews: Record<string, HistoryImageState>;
		loading: boolean;
		refreshing: boolean;
		historyLoaded: boolean;
		total: number;
		pageSize: number;
		columns: number;
		rows: number;
		query: string;
		activating: string | null;
		selectedIndex: number;
		historyGrid: HTMLElement | null;
		historyWorkspace: HTMLElement | null;
		onClearSearch: () => void;
		onActivate: (item: HistoryItem, index: number) => void;
		onPin: (item: HistoryItem, index: number) => void;
		onDelete: (item: HistoryItem, index: number) => void;
		onFilterSource: (item: HistoryItem) => void;
		onNavigate: (event: KeyboardEvent, index: number) => boolean;
		onRequestImage: (item: HistoryItem) => void;
	} = $props();

	const skeletonIndexes = $derived(Array.from({ length: pageSize }, (_, index) => index));
	let now = $state(Date.now());

	function shortMimeType(item: HistoryItem) {
		return (item.mimeTypes[0] ?? 'unknown type').split(';')[0];
	}

	function historyItemLabel(item: HistoryItem) {
		return [
			`Activate clipboard record: ${item.preview}`,
			item.pinned ? 'Pinned' : null,
			shortMimeType(item),
			formatBytes(item.logicalSize),
			`from ${item.sourceDevice || 'an unknown device'}`,
			formatAge(item.physicalMillis, now)
		]
			.filter(Boolean)
			.join(', ');
	}

	function selectOnPointerEnter(index: number) {
		if (historyGrid?.contains(document.activeElement)) return;
		selectedIndex = index;
	}

	onMount(() => {
		const timer = window.setInterval(() => (now = Date.now()), 30_000);
		return () => window.clearInterval(timer);
	});
</script>

<main class="history-workspace" bind:this={historyWorkspace}>
	<div class="history-scroll" aria-busy={loading || refreshing}>
		{#if loading}
			<div
				class="history-grid loading-grid"
				style:--history-columns={columns}
				style:--history-rows={rows}
				aria-label="Loading clipboard history"
			>
				{#each skeletonIndexes as index (index)}
					<article
						class="history-cell loading-cell"
						aria-label={`Loading history item ${index + 1}`}
					>
						<Skeleton class="preview-skeleton" />
						<Skeleton class="meta-skeleton" />
					</article>
				{/each}
			</div>
		{:else if historyLoaded && total === 0}
			<Empty class="empty-register">
				<EmptyMedia><Clipboard aria-hidden="true" /></EmptyMedia>
				<EmptyHeader>
					<EmptyTitle>{query.trim() ? 'No matching records' : 'No retained records'}</EmptyTitle>
					<EmptyDescription>
						{query.trim()
							? 'Clear or revise the search, or copy something on a connected device.'
							: 'Copy something on this or a connected device to add it to retained history.'}
					</EmptyDescription>
				</EmptyHeader>
				{#if query.trim()}
					<Button variant="outline" size="sm" onclick={onClearSearch}>Clear search</Button>
				{/if}
			</Empty>
		{:else if historyLoaded}
			<div
				class="history-grid"
				bind:this={historyGrid}
				style:--history-columns={columns}
				style:--history-rows={rows}
				role="list"
				aria-label="Clipboard history results"
			>
				{#each items as item, index (item.contentId)}
					<article class="history-cell-shell" role="listitem">
						<ContextMenu>
							<ContextMenuTrigger
								class="history-context-trigger"
								oncontextmenu={() => (selectedIndex = index)}
							>
								<Button
									variant="ghost"
									class="history-cell"
									data-selected={index === selectedIndex}
									tabindex={index === selectedIndex ? 0 : -1}
									disabled={activating !== null}
									aria-label={historyItemLabel(item)}
									aria-keyshortcuts="Enter ArrowUp ArrowDown ArrowLeft ArrowRight H J K L Home End"
									onfocus={() => (selectedIndex = index)}
									onpointerenter={() => selectOnPointerEnter(index)}
									onkeydown={(event) => onNavigate(event, index)}
									onclick={() => onActivate(item, index)}
								>
									{#if historyItemHasImage(item)}
										<HistoryImagePreview
											previewState={previews[item.contentId]}
											alt={`Clipboard image from ${item.sourceDevice || 'an unknown device'}`}
											onVisible={() => onRequestImage(item)}
										/>
									{:else}
										<span class="record-content">{item.preview}</span>
									{/if}
									<span class="record-footer">
										<span class="record-metadata">
											{#if item.pinned}<Pin aria-label="Pinned" />{/if}
											<span>{shortMimeType(item)}</span>
											<span>{formatBytes(item.logicalSize)}</span>
											<span>{item.sourceDevice || 'unknown device'}</span>
											<time
												datetime={new Date(item.physicalMillis).toISOString()}
												title={new Date(item.physicalMillis).toLocaleString()}
											>
												{formatAge(item.physicalMillis, now)}
											</time>
										</span>
										<span class="activate-cue" aria-hidden="true">
											{activating === item.contentId ? '…' : '↵'}
										</span>
									</span>
								</Button>
							</ContextMenuTrigger>
							<ContextMenuContent class="history-context-menu">
								<ContextMenuLabel class="history-context-label">Record {index + 1}</ContextMenuLabel
								>
								<ContextMenuItem
									disabled={activating !== null}
									onSelect={() => onActivate(item, index)}
								>
									<Play aria-hidden="true" /> Activate & close
									<ContextMenuShortcut>Enter</ContextMenuShortcut>
								</ContextMenuItem>
								<ContextMenuItem disabled={activating !== null} onSelect={() => onPin(item, index)}>
									{#if item.pinned}<PinOff aria-hidden="true" /> Unpin{:else}<Pin
											aria-hidden="true"
										/> Pin{/if}
								</ContextMenuItem>
								<ContextMenuItem
									disabled={activating !== null || (!item.sourceDevice && !item.sourceNode)}
									onSelect={() => onFilterSource(item)}
								>
									<Filter aria-hidden="true" /> Show only this device
								</ContextMenuItem>
								<ContextMenuSeparator />
								<ContextMenuItem
									variant="destructive"
									disabled={activating !== null}
									onSelect={() => onDelete(item, index)}
								>
									<Trash2 aria-hidden="true" /> Delete from mesh
								</ContextMenuItem>
							</ContextMenuContent>
						</ContextMenu>
					</article>
				{/each}
			</div>
		{/if}
	</div>
</main>
