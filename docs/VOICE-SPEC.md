# Escouade — mode vocal (spec)

Objectif : coder à l'oral, Mac posé, sans toucher au clavier. Escouade écoute en fond,
transcrit, envoie le prompt à l'agent actif avec le contexte visuel de l'écran, et prévient
(son / voix) quand l'agent a fini.

## Flux

```
micro (cpal) ─► VAD ─► whisper.cpp (local) ─► détecteur début/fin ─► prompt
                                                     │
écran (frames pendant la dictée) ─► sélection ───────┘─► agent actif (stream-json + images)
                                                                │
                                         fin de tour ─► carillon (+ `say` optionnel)
```

## Phases

### Phase 1 — Appuyer pour parler (valider la transcription)
- Rust : capture micro avec `cpal`, 16 kHz mono, tampon circulaire.
- Transcription : `whisper-rs`, modèle `large-v3-turbo` (q5), Metal. Modèle téléchargé au
  premier usage dans `~/.escouade/models/`, jamais embarqué dans le bundle.
- Raccourci global `⌥Espace` maintenu = enregistrement ; relâché = transcription.
- Le texte arrive dans le champ de saisie de l'agent actif (pas d'envoi auto en phase 1).
- `Info.plist` : `NSMicrophoneUsageDescription`.
- Réglage : activer/désactiver le mode vocal, choix du modèle, langue (fr par défaut).

### Phase 2 — Contexte visuel : « enregistrement » de l'écran
Claude Code ne lit pas la vidéo : on enregistre des frames et on n'envoie que les utiles.
- Pendant la dictée : capture de la fenêtre au premier plan toutes les ~700 ms
  (ScreenCaptureKit, ou `screencapture -x -l<windowId>` pour le MVP), avec position du curseur.
- Chaque frame est horodatée et alignée sur les mots transcrits (timestamps Whisper).
- Sélection (déterministe, sans LLM) :
  1. la frame au moment de chaque mot déictique (« ça », « ici », « ce bouton », « là ») ;
  2. les frames où l'écran change nettement (différence de hash perceptuel > seuil) ;
  3. la dernière frame.
  Plafond : 6 images (environ 1,5 k tokens chacune), réduites à 1280 px de large, curseur
  dessiné dessus.
- Le prompt envoyé :
  ```
  <transcription>
  [Contexte : N captures de l'écran pendant que je parlais, dans l'ordre ;
   le curseur rouge indique où je pointais. URL : http://localhost:5173/…]
  ```
- URL : lue depuis le navigateur au premier plan (AppleScript Chrome/Safari) si c'est un
  localhost → l'agent peut vérifier lui-même avec Playwright / Chrome DevTools MCP.
- `Info.plist` : autorisation d'enregistrement de l'écran.

### Phase 3 — Mains libres
- VAD (Silero ou VAD de whisper.cpp) : on ne transcrit que quand quelqu'un parle.
- Mot de réveil « Escouade » → démarre la dictée (son court de confirmation).
- « envoie » en fin de phrase → envoi. « annule » → abandon. Silence > 8 s sans « envoie »
  → le texte reste en brouillon dans le champ, rien n'est envoyé.
- Le texte transcrit s'affiche en direct dans le champ (on voit ce qui partira).
- Icône dans la barre de statut : écoute / dictée / envoi.

### Phase 4 — Retour vocal
- Fin de tour : carillon existant (`notify::play_chime`).
- Option : lecture de la 1re phrase de la réponse via `say -v Thomas`.
- Le micro est coupé pendant la lecture (sinon il se transcrit lui-même).

## Hors périmètre (pour l'instant)
- Transcription cloud (coût d'une écoute permanente).
- Vidéo envoyée telle quelle au modèle.

## Critères d'acceptation
- Phase 1 : 10 phrases de dev en français transcrites sans erreur de sens, en moins de 2 s après
  le relâchement de la touche, sur Apple Silicon.
- Phase 2 : « ce bouton-là est mal aligné » en pointant un bouton → la capture jointe montre
  ce bouton sous le curseur.
- Phase 3 : 5 minutes de conversation dans la pièce sans « Escouade » → aucun envoi.
