use rand::prelude::IndexedRandom;
use rand::rngs::StdRng;
use rand::{RngExt, SeedableRng};

const REGIONS: &[&str] = &[
    "US-West", "US-East", "US-Central", "EU-West", "EU-North", "EU-Central",
    "APAC-East", "APAC-South", "LATAM-North", "LATAM-South", "MEA-West",
    "MEA-East", "CAN-West", "CAN-East", "ANZ-South", "NORDIC-North",
    "IBERIA-West", "DACH-Central", "SEA-South", "INDIA-Central",
];
const COUNTRIES: &[&str] = &[
    "United States", "Canada", "United Kingdom", "Germany", "France",
    "Japan", "Australia", "Brazil", "India", "Netherlands",
];
const FIRST_NAMES: &[&str] = &[
    "Alice", "Bob", "Charlie", "Diana", "Edward", "Fiona", "George", "Hannah",
    "Ivan", "Julia", "Kevin", "Laura", "Michael", "Nina", "Oscar", "Patricia",
    "Quinn", "Rachel", "Samuel", "Tina",
];
const LAST_NAMES: &[&str] = &[
    "Smith", "Johnson", "Williams", "Brown", "Jones", "Garcia", "Miller",
    "Davis", "Rodriguez", "Martinez", "Wilson", "Anderson", "Taylor", "Thomas",
    "Moore", "Jackson", "Martin", "Lee", "Thompson", "White",
];
const ROLES: &[&str] = &[
    "Engineer", "Designer", "Manager", "Analyst", "Director",
    "Consultant", "Coordinator", "Specialist",
];
const CATEGORIES: &[&str] = &["Infrastructure", "Mobile", "Web", "Analytics", "Platform", "Security"];
const DIVISIONS: &[&str] = &["Engineering", "Product", "Marketing", "Sales", "Operations", "Research"];
const PRIORITIES: &[&str] = &["low", "medium", "high"];
const STATUS_TASK: &[&str] = &["completed", "completed", "completed", "pending", "pending", "blocked", "overdue"];

pub struct DomainGenerator {
    rng: StdRng,
}

pub struct CompanyHierarchy {
    pub company_code: String,
    pub company_query: String,
    pub projects: Vec<ProjectHierarchy>,
    pub total_records: u32,
}

pub struct ProjectHierarchy {
    pub project_id: String,
    pub project_query: String,
    pub task_batch: String,
    pub task_count: u32,
}

impl DomainGenerator {
    pub fn new(seed: u64) -> Self {
        Self { rng: StdRng::seed_from_u64(seed) }
    }

    fn pick<'a>(&mut self, list: &[&'a str]) -> &'a str {
        list.choose(&mut self.rng).unwrap()
    }

    pub fn generate_company_hierarchy(&mut self, company_id: u32) -> CompanyHierarchy {
        let code = format!("COM-{company_id:07}");

        let first = self.pick(FIRST_NAMES);
        let last = self.pick(LAST_NAMES);
        let region = self.pick(REGIONS);
        let country = self.pick(COUNTRIES);
        let role = self.pick(ROLES);
        let priority = self.pick(PRIORITIES);
        let revenue: u32 = self.rng.random_range(5000..150000);
        let postal: u32 = self.rng.random_range(1000..99999);
        let y = self.rng.random_range(1960u32..2000);
        let m = self.rng.random_range(1u32..13);
        let d = self.rng.random_range(1u32..29);
        let jy = self.rng.random_range(2020u32..2027);
        let jm = self.rng.random_range(1u32..13);
        let jd = self.rng.random_range(1u32..29);

        let headcount: u32 = self.rng.random_range(1u32..999);

        let company_query = format!(
            r#"PUT {{_type: "Company", code: "{code}", name: "{first} {last}", region: "{region}", country: "{country}", role: "{role}", email: "c{company_id}@mail.com", phone: "+1{:08}", address: "123 Main St", postal_code: "{postal:05}", joined_date: @"{jy}-{jm:02}-{jd:02}", status: "active", priority: "{priority}", revenue: {revenue}, headcount: {headcount}}} IN "catalog""#,
            company_id % 100_000_000,
        );

        // Projects: 1-3 (geometric, mean ~1.5)
        let num_projects: u32 = if self.rng.random_bool(0.5) { 1 }
            else if self.rng.random_bool(0.7) { 2 }
            else { 3 };

        let mut projects = Vec::new();
        let mut total = 1u32; // company record

        for ci in 0..num_projects {
            let project_id = format!("PRJ-{company_id:07}-{ci}");
            let budget: u32 = self.rng.random_range(10000..500000);
            let duration: u32 = *[12, 24, 36, 48].choose(&mut self.rng).unwrap();
            let rate: f64 = self.rng.random_range(12..36) as f64 / 100.0;
            let category = self.pick(CATEGORIES);
            let division = self.pick(DIVISIONS);
            let ms = self.rng.random_range(1u32..13);

            let project_query = format!(
                r#"PUT {{_type: "Project", project_id: "{project_id}", category: "{category}", budget: {budget}, duration: {duration}, rate: {rate:.2}, start_date: @"2025-{ms:02}-15", status: "active", division: "{division}", grade: "A"}} IN "catalog" LINK TO "catalog" WHERE code = "{code}" AS "owner""#
            );

            // Tasks as batch
            let estimated = budget as f64 / duration as f64;
            let hourly_rate = (budget as f64 * rate) / 12.0;
            let mut batch = format!(r#"PUT BATCH IN "catalog" ["#);
            for task_n in 1..=duration {
                if task_n > 1 { batch.push_str(", "); }
                let actual_hours = estimated + hourly_rate;
                let month = ((task_n - 1) % 12) + 1;
                let status = self.pick(STATUS_TASK);
                let days_overdue: u32 = if status == "blocked" || status == "overdue" {
                    self.rng.random_range(1..180)
                } else { 0 };
                let task_priority = self.pick(PRIORITIES);

                batch.push_str(&format!(
                    r#"{{_type: "Task", numero: {task_n}, estimated_hours: {estimated:.2}, actual_hours: {actual_hours:.2}, due_date: @"2026-{month:02}-25", status: "{status}", days_overdue: {days_overdue}, priority: "{task_priority}", description: "Task {task_n}"}}"#
                ));
            }
            batch.push_str(&format!(
                r#"] LINK TO "catalog" WHERE project_id = "{project_id}" AS "task_of""#
            ));

            total += 1 + duration; // project + tasks
            projects.push(ProjectHierarchy {
                project_id,
                project_query,
                task_batch: batch,
                task_count: duration,
            });
        }

        CompanyHierarchy { company_code: code, company_query, projects, total_records: total }
    }

    /// Estimate total records for N companies.
    pub fn estimate_records(companies: u32) -> u64 {
        // avg: 1 company + 1.5 projects + 1.5*30 tasks = ~46.5 records
        (companies as f64 * 46.5) as u64
    }
}

fn random_upper_char(rng: &mut StdRng) -> char {
    (b'A' + (rng.random_range(0u8..26))) as char
}
