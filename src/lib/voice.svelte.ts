// Voice mode (macOS): state shown in the UI, and the dictation the backend hands to the composer.

import { invoke } from '@tauri-apps/api/core';
import { listen } from '@tauri-apps/api/event';
import { app } from './state.svelte';
import type { Attachment } from './types';

export type VoiceState = 'off' | 'idle' | 'listening' | 'recording' | 'transcribing' | 'dictating' | 'sending';

/** What the composer of the active agent receives. */
export type VoiceInput =
  | { kind: 'append'; text: string }
  | { kind: 'clear' }
  | { kind: 'finish'; text: string; context: string | null; frames: Attachment[]; send: boolean };

type VoiceEvent =
  | { kind: 'state'; state: VoiceState }
  | { kind: 'model'; status: 'downloading' | 'ready' | 'error'; progress: number }
  | { kind: 'notice'; message: string }
  | { kind: 'error'; message: string }
  | VoiceInput;

interface VoiceStatus {
  supported: boolean;
  modelReady: boolean;
  downloading: boolean;
  state: VoiceState;
}

class VoiceStore {
  /** False off macOS (or when the backend has no voice mode): the UI hides it. */
  supported = $state(false);
  state = $state<VoiceState>('off');
  modelReady = $state(false);
  downloading = $state(false);
  progress = $state(0);
}

export const voice = new VoiceStore();

const handlers = new Set<(i: VoiceInput) => void>();

/** The composer registers here; it applies the input only if it belongs to the active agent. */
export function onVoiceInput(h: (i: VoiceInput) => void): () => void {
  handlers.add(h);
  return () => handlers.delete(h);
}

export const VOICE_LABELS: Record<VoiceState, string> = {
  off: '',
  idle: 'Voix prête',
  listening: 'Écoute…',
  recording: 'Enregistrement…',
  transcribing: 'Transcription…',
  dictating: 'Dictée…',
  sending: 'Envoi…',
};

function onEvent(e: VoiceEvent) {
  switch (e.kind) {
    case 'state':
      voice.state = e.state;
      break;
    case 'model':
      voice.downloading = e.status === 'downloading';
      voice.progress = e.progress;
      if (e.status === 'ready') {
        voice.modelReady = true;
        app.toast('Modèle vocal téléchargé.', 'ok');
      }
      break;
    case 'notice':
      app.toast(e.message, 'info');
      break;
    case 'error':
      app.toast(e.message, 'error');
      break;
    default:
      for (const h of handlers) h(e);
  }
}

export async function initVoice() {
  try {
    const s = await invoke<VoiceStatus | null>('voice_status');
    if (!s?.supported) return;
    voice.supported = true;
    voice.state = s.state;
    voice.modelReady = s.modelReady;
    voice.downloading = s.downloading;
    await listen<VoiceEvent>('voice', (ev) => onEvent(ev.payload));
  } catch {
    // No voice mode: the rest of the app does not depend on it.
  }
}

export const downloadVoiceModel = () => invoke<void>('voice_download_model');
