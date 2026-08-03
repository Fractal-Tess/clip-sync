<script lang="ts">
	import { onMount } from 'svelte';

	import {
		cancelTransfer,
		forgetDevice,
		getDiagnostics,
		getPeers,
		getSettings,
		getTransfers,
		updatePeerInterfaces,
		updateSharedSetting,
		type Diagnostic,
		type Peers,
		type Settings,
		type SharedSetting,
		type Transfer
	} from '$lib/bridge';
	import type { ControlSection } from '$lib/control-center';
	import DiagnosticsPanel from './diagnostics-panel.svelte';
	import PeersPanel from './peers-panel.svelte';
	import SettingsPanel from './settings-panel.svelte';
	import TransfersPanel from './transfers-panel.svelte';

	let { section }: { section: Exclude<ControlSection, 'history'> } = $props();
	let peers = $state.raw<Peers | null>(null);
	let settings = $state.raw<Settings | null>(null);
	let diagnostics = $state.raw<Diagnostic[] | null>(null);
	let transfers = $state.raw<Transfer[] | null>(null);
	let loading = $state(false);
	let polling = false;
	let error = $state<string | null>(null);
	let notice = $state<string | null>(null);
	let generation = 0;

	function errorMessage(cause: unknown) {
		return cause instanceof Error ? cause.message : String(cause);
	}

	async function load(target = section, background = false) {
		if (background) {
			if (polling || loading) return;
			polling = true;
		} else {
			loading = true;
			error = null;
		}
		const requestGeneration = ++generation;
		try {
			switch (target) {
				case 'peers': {
					const nextPeers = await getPeers();
					if (requestGeneration === generation) peers = nextPeers;
					break;
				}
				case 'settings': {
					const nextSettings = await getSettings();
					if (requestGeneration === generation) settings = nextSettings;
					break;
				}
				case 'diagnostics': {
					const nextDiagnostics = await getDiagnostics();
					if (requestGeneration === generation) diagnostics = nextDiagnostics;
					break;
				}
				case 'transfers': {
					const nextTransfers = await getTransfers();
					if (requestGeneration === generation) transfers = nextTransfers;
					break;
				}
			}
		} catch (cause) {
			if (!background && requestGeneration === generation) error = errorMessage(cause);
		} finally {
			if (background) polling = false;
			else if (requestGeneration === generation) loading = false;
		}
	}

	async function mutate(task: () => Promise<{ ok: boolean; message: string }>) {
		const activeSection = section;
		const mutationGeneration = ++generation;
		loading = true;
		error = null;
		notice = null;
		try {
			const result = await task();
			if (mutationGeneration !== generation) return;
			if (!result.ok) throw new Error(result.message || 'The daemon rejected the request');
			notice = result.message;
			await load(activeSection);
		} catch (cause) {
			if (mutationGeneration !== generation) return;
			error = errorMessage(cause);
			loading = false;
		}
	}

	onMount(() => {
		const activeSection = section;
		void load(activeSection);
		const interval = window.setInterval(
			() => {
				if (!loading && !polling && document.visibilityState === 'visible') {
					void load(activeSection, true);
				}
			},
			activeSection === 'transfers' ? 500 : 5_000
		);
		return () => {
			generation += 1;
			window.clearInterval(interval);
		};
	});
</script>

<main class="management-workspace" aria-busy={loading}>
	{#if error}
		<div class="management-banner error" role="alert">
			<span>{error}</span>
			<button type="button" onclick={() => void load()}>Retry</button>
		</div>
	{:else if notice}
		<div class="management-banner" role="status">{notice}</div>
	{/if}

	{#if section === 'peers' && peers}
		<PeersPanel
			{peers}
			busy={loading}
			onRefresh={() => void load('peers')}
			onForget={(deviceId) => void mutate(() => forgetDevice(deviceId))}
		/>
	{:else if section === 'settings' && settings}
		<SettingsPanel
			{settings}
			busy={loading}
			onRefresh={() => void load('settings')}
			onUpdate={(setting: SharedSetting, value: number) =>
				void mutate(() => updateSharedSetting(setting, value))}
			onUpdatePeerInterfaces={(interfaces) => void mutate(() => updatePeerInterfaces(interfaces))}
		/>
	{:else if section === 'diagnostics' && diagnostics}
		<DiagnosticsPanel
			checks={diagnostics}
			busy={loading}
			onRefresh={() => void load('diagnostics')}
		/>
	{:else if section === 'transfers' && transfers}
		<TransfersPanel
			{transfers}
			busy={loading}
			onRefresh={() => void load('transfers')}
			onCancel={(transferId) => void mutate(() => cancelTransfer(transferId))}
		/>
	{:else if loading}
		<div class="management-loading" aria-label={`Loading ${section}`}>
			<span></span><span></span><span></span>
		</div>
	{/if}
</main>
