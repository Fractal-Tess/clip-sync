export const controlSections = [
	{ id: 'history', label: 'History' },
	{ id: 'transfers', label: 'Transfers' },
	{ id: 'peers', label: 'Peers' },
	{ id: 'settings', label: 'Settings' },
	{ id: 'diagnostics', label: 'Diagnostics' }
] as const;

export type ControlSection = (typeof controlSections)[number]['id'];
