import { useEffect, useRef, useState, type FormEvent, type KeyboardEvent } from "react";

type ChatMessage = { role: "user" | "assistant"; content: string };

type ChatPageProps = {
  baseUrl: string;
  model?: string;
  authenticationEnabled: boolean;
  complete(request: { model: string; messages: ChatMessage[]; token?: string }): Promise<string>;
};

export default function ChatPage({ baseUrl, model, authenticationEnabled, complete }: ChatPageProps) {
  const [messages, setMessages] = useState<ChatMessage[]>([]);
  const [draft, setDraft] = useState("");
  const [token, setToken] = useState("");
  const [error, setError] = useState("");
  const [sending, setSending] = useState(false);
  const endRef = useRef<HTMLDivElement>(null);

  useEffect(() => {
    endRef.current?.scrollIntoView?.({ behavior: "smooth" });
  }, [messages, sending, error]);

  const send = async (event?: FormEvent) => {
    event?.preventDefault();
    const content = draft.trim();
    if (!content || sending || !model) return;

    const nextMessages: ChatMessage[] = [...messages, { role: "user", content }];
    setMessages(nextMessages);
    setDraft("");
    setError("");
    setSending(true);

    try {
      const reply = (await complete({
        model,
        messages: nextMessages,
        token: authenticationEnabled && token.trim() ? token.trim() : undefined,
      })).trim();
      if (!reply) throw new Error("The server returned an empty assistant response.");
      setMessages(current => [...current, { role: "assistant", content: reply }]);
    } catch (cause) {
      setError(cause instanceof Error ? cause.message : String(cause || "The request failed. Please try again."));
    } finally {
      setSending(false);
    }
  };

  const handleKeyDown = (event: KeyboardEvent<HTMLTextAreaElement>) => {
    if (event.key === "Enter" && !event.shiftKey) {
      event.preventDefault();
      void send();
    }
  };

  if (!model) return <section className="view chat-page chat-unavailable">
    <div className="empty"><i>◇</i><b>No model is running</b><span>Load a model from the Models page before starting a chat.</span></div>
  </section>;

  return <section className="view chat-page">
    <header className="chat-head">
      <div><h1>Chat</h1><p>Send a message through the active local OpenAI-compatible API.</p></div>
      <div className="chat-connection"><i className="dot run" /><span>{model}</span><code>{baseUrl}</code></div>
    </header>
    <div className="chat-messages scroll" aria-live="polite">
      {messages.length === 0 && <div className="chat-welcome"><span className="chat-mark">✦</span><h2>Start a local conversation</h2><p>Messages are sent directly to <b>{model}</b> on your active server.</p></div>}
      {messages.map((message, index) => <article className={`chat-message ${message.role}`} key={index}><span>{message.role === "user" ? "You" : model}</span><p>{message.content}</p></article>)}
      {sending && <article className="chat-message assistant loading" aria-label="Assistant is responding"><span>{model}</span><div><i /><i /><i /></div></article>}
      {error && <div className="chat-error" role="alert"><b>Request failed</b><span>{error}</span></div>}
      <div ref={endRef} />
    </div>
    <form className="chat-composer" onSubmit={send}>
      {authenticationEnabled && <label className="chat-token"><span>API token</span><input type="password" value={token} onChange={event => setToken(event.target.value)} placeholder="Paste a Bearer token" autoComplete="off" /></label>}
      <div className="chat-input"><textarea aria-label="Message" value={draft} onChange={event => setDraft(event.target.value)} onKeyDown={handleKeyDown} placeholder={`Message ${model}`} rows={2} /><button className="btn primary" type="submit" disabled={!draft.trim() || sending}>Send</button></div>
      <small>Enter to send · Shift+Enter for a new line</small>
    </form>
  </section>;
}
