export class PublishedDirectoryDurabilityError extends Error {
  constructor(outputPath, identity, cause) {
    super(
      `output was published but parent-directory durability is uncertain: ${outputPath}`,
      { cause },
    );
    this.name = "PublishedDirectoryDurabilityError";
    this.code = "HYPER_PUBLISHED_DIRECTORY_DURABILITY_UNCERTAIN";
    this.output_path = outputPath;
    this.identity = identity;
    this.published = true;
    this.durability = "uncertain";
  }
}
