import { Routes } from '@angular/router';

export const routes: Routes = [
  { path: 'admin', loadComponent: () => import('./admin/admin.component').then(m => m.AdminComponent) },
  { path: 'dashboard', loadChildren: () => import('./dashboard/dashboard.routes').then(m => m.dashboardRoutes) },
];
