/**
 * Merge a Settings snapshot by durable project ID. Names are display fields and
 * may be duplicated; an edited row is retained only when its ID is present.
 */
export function mergeSettingsProjects(initialProjects, currentProjects, editedProjects) {
  const initialIds = new Set(initialProjects.map((project) => project.id).filter(Boolean));
  const editedIds = new Set(editedProjects.map((project) => project.id).filter(Boolean));
  const projects = currentProjects.filter(
    (project) => !initialIds.has(project.id) || editedIds.has(project.id),
  );
  const currentIds = new Set(projects.map((project) => project.id).filter(Boolean));
  for (const project of editedProjects) {
    if (!project.id || !currentIds.has(project.id)) {
      projects.push(project);
      if (project.id) currentIds.add(project.id);
    }
  }
  return projects;
}
