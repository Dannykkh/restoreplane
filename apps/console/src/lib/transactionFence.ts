type ReadTicket = { binding: string; revision: number; sequence: number };
type MutationTicket = { binding: string; sequence: number };

/** Orders UI requests; it never supplies evidence of a DB commit or rollback. */
export class TransactionFence {
  private binding = "";
  private revision = 0;
  private reads = 0;
  private mutations = 0;
  private pending = 0;
  bind(binding: string): void {
    if (this.binding === binding) return;
    this.binding = binding; this.pending = 0; this.invalidate();
  }
  invalidate(): void { this.revision++; this.reads++; }
  read(): ReadTicket | null {
    return this.pending ? null : { binding: this.binding, revision: this.revision, sequence: ++this.reads };
  }
  acceptsRead(ticket: ReadTicket): boolean {
    return !this.pending && ticket.binding === this.binding && ticket.revision === this.revision && ticket.sequence === this.reads;
  }
  mutate(): MutationTicket | null {
    if (this.pending) return null;
    this.invalidate(); this.pending = ++this.mutations;
    return { binding: this.binding, sequence: this.pending };
  }
  acceptsMutation(ticket: MutationTicket): boolean {
    return ticket.binding === this.binding && ticket.sequence === this.pending;
  }
  finish(ticket: MutationTicket): void {
    if (this.acceptsMutation(ticket)) { this.pending = 0; this.invalidate(); }
  }
}
