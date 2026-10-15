import { format } from "./b";
import { Queue } from "./queue";

export function run(): string {
  const q = new Queue();
  q.push(1);
  return format(" x ");
}
