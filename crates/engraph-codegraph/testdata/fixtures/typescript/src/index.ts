export { Book, Genre, Loan, Member, Shelf } from "./models";
export type { Describable } from "./models";
export {
  Library,
  averagePages,
  describeAll,
  longestTitle,
  merge,
  scienceBooks,
  starterLibrary,
  summary,
} from "./ops";
export { capitalize, initials, pluralize, slug, titleCase, truncate } from "./text";
