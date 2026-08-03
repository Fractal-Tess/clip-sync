<script lang="ts">
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
		title,
		description,
		confirmLabel,
		busy,
		destructive = false,
		onConfirm,
		onCancel
	}: {
		title: string;
		description: string;
		confirmLabel: string;
		busy: boolean;
		destructive?: boolean;
		onConfirm: () => void;
		onCancel: () => void;
	} = $props();
</script>

<AlertDialog open={true} onOpenChange={(open) => !open && !busy && onCancel()}>
	<AlertDialogContent>
		<AlertDialogHeader>
			<AlertDialogTitle>{title}</AlertDialogTitle>
			<AlertDialogDescription>{description}</AlertDialogDescription>
		</AlertDialogHeader>
		<AlertDialogFooter>
			<AlertDialogCancel disabled={busy} onclick={onCancel}>Cancel</AlertDialogCancel>
			<Button variant={destructive ? 'destructive' : 'default'} disabled={busy} onclick={onConfirm}>
				{confirmLabel}
			</Button>
		</AlertDialogFooter>
	</AlertDialogContent>
</AlertDialog>
