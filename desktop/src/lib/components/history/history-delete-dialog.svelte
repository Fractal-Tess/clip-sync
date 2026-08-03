<script lang="ts">
	import type { HistoryItem } from '$lib/bridge';
	import {
		AlertDialog,
		AlertDialogCancel,
		AlertDialogContent,
		AlertDialogDescription,
		AlertDialogFooter,
		AlertDialogHeader,
		AlertDialogTitle
	} from '$lib/components/ui/alert-dialog';
	import { Button } from '$lib/components/ui/button';

	let {
		item,
		busy,
		onConfirm,
		onCancel
	}: {
		item: HistoryItem;
		busy: boolean;
		onConfirm: () => void;
		onCancel: () => void;
	} = $props();
</script>

<AlertDialog open={true} onOpenChange={(open) => !open && !busy && onCancel()}>
	<AlertDialogContent class="history-delete-dialog">
		<AlertDialogHeader>
			<AlertDialogTitle>Delete from every mesh device?</AlertDialogTitle>
			<AlertDialogDescription>
				This replicated deletion removes the retained record everywhere. It does not alter copies
				already pasted into other applications.
			</AlertDialogDescription>
		</AlertDialogHeader>
		<blockquote>{item.preview || item.mimeTypes.join(', ')}</blockquote>
		<AlertDialogFooter>
			<AlertDialogCancel disabled={busy} onclick={onCancel}>Keep record</AlertDialogCancel>
			<Button variant="destructive" disabled={busy} onclick={onConfirm}>
				{busy ? 'Deleting…' : 'Delete from mesh'}
			</Button>
		</AlertDialogFooter>
	</AlertDialogContent>
</AlertDialog>
