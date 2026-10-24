import { createRoot } from 'react-dom/client'

import App from './App.tsx'
import './globals.css'
import './lib/outboundParser.js'
import { initVisibilityReload } from './lib/visibilityReload'

initVisibilityReload()

if (import.meta.env.PROD && 'serviceWorker' in navigator) {
  window.addEventListener('load', () => {
    void navigator.serviceWorker.register('/sw.js').catch((error: unknown) => {
      console.error('Failed to register the service worker:', error)
    })
  })
}

createRoot(document.getElementById('root')!).render(<App />)
