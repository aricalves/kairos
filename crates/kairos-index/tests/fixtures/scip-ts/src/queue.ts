export class Queue {
  items: number[] = [];

  push(n: number): void {
    this.items.push(n);
  }
}
