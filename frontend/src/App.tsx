import { useState } from 'react'

function App() {
  const [status] = useState<string>('model-serving tooling skeleton')

  return (
    <main className="shell">
      <h1>model-serving</h1>
      <p className="status">{status}</p>
      <p className="hint">
        Web UI (Dashboard / Models / Instances / Settings / Logs) is built in
        M7. This page only proves the Vite + React + TypeScript toolchain.
      </p>
    </main>
  )
}

export default App